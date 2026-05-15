use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use github_app::GitHubAppTokenProvider;
use http::{HeaderName, HeaderValue};
use rmcp::{
    model::{CallToolRequestParams, CallToolResult, JsonObject, RawContent},
    service::RunningService,
    transport::streamable_http_client::{
        StreamableHttpClientTransportConfig, StreamableHttpClientWorker,
    },
    RoleClient, ServiceExt,
};
use tokio::sync::RwLock;

use crate::auth::AuthSubject;
use crate::primitives::AuthScope;

use super::super::{
    McpAuthMode, McpUpstreamConfig, SearchableToolSet, ToolSetEntry, ToolSetsError,
};

const GITHUB_APP_REFRESH_INTERVAL: Duration = Duration::from_secs(50 * 60);

pub struct UpstreamToolSet {
    name: String,
    tool_prefix: String,
    category: String,
    category_description: String,
    /// Empty = unrestricted.
    required_scopes: Vec<AuthScope>,
    /// When true, hidden from non-agent subjects (Users, ExportedAgents,
    /// Anonymous). See [`McpUpstreamConfig::internal_only`].
    internal_only: bool,
    tools: Vec<ToolSetEntry>,
    client: Arc<RwLock<RunningService<RoleClient, ()>>>,
    _refresh_task: Option<tokio::task::JoinHandle<()>>,
}

impl UpstreamToolSet {
    pub(in super::super) async fn init(
        upstream: &McpUpstreamConfig,
        github_app: Option<&Arc<GitHubAppTokenProvider>>,
    ) -> Result<UpstreamToolSet, ToolSetsError> {
        let header_value = match upstream.auth_mode {
            McpAuthMode::Static => {
                if upstream.auth_header.is_empty() {
                    if upstream.auth_required {
                        let env_key = format!("{}_AUTH_HEADER", upstream.name.to_uppercase());
                        return Err(ToolSetsError::MissingAuthHeader {
                            name: upstream.name.clone(),
                            env_key,
                        });
                    }
                    String::new()
                } else {
                    upstream.auth_header.clone()
                }
            }
            McpAuthMode::GithubApp => {
                let provider = github_app
                    .ok_or_else(|| ToolSetsError::GithubAppNotConfigured(upstream.name.clone()))?;
                let token = provider.generate_token().await?;
                format!("Bearer {}", token.token)
            }
        };

        let client = build_rmcp_client(upstream, &header_value).await?;

        let allowed = upstream.allowed_tools.as_ref();
        let tools: Vec<ToolSetEntry> = client
            .list_all_tools()
            .await?
            .into_iter()
            .filter(|t| {
                allowed
                    .map(|list| list.iter().any(|a| a == t.name.as_ref()))
                    .unwrap_or(true)
            })
            .map(|description| ToolSetEntry {
                name: description.name.to_string(),
                description,
            })
            .collect();

        let client = Arc::new(RwLock::new(client));

        let refresh_task = match upstream.auth_mode {
            McpAuthMode::GithubApp => {
                let provider = Arc::clone(github_app.expect("checked above"));
                let upstream = upstream.clone();
                let client_for_task = Arc::clone(&client);
                Some(tokio::spawn(async move {
                    refresh_loop(upstream, provider, client_for_task).await;
                }))
            }
            McpAuthMode::Static => None,
        };

        let tool_prefix = upstream
            .tool_prefix
            .clone()
            .unwrap_or_else(|| upstream.name.clone());

        Ok(UpstreamToolSet {
            name: upstream.name.clone(),
            tool_prefix,
            category: upstream.category.clone().unwrap_or_default(),
            category_description: upstream.category_description.clone().unwrap_or_default(),
            required_scopes: upstream.required_scopes.clone().unwrap_or_default(),
            internal_only: upstream.internal_only,
            tools,
            client,
            _refresh_task: refresh_task,
        })
    }
}

async fn build_rmcp_client(
    upstream: &McpUpstreamConfig,
    header_value: &str,
) -> Result<RunningService<RoleClient, ()>, ToolSetsError> {
    let mut headers = HashMap::new();
    if !header_value.is_empty() {
        headers.insert(
            HeaderName::from_bytes(upstream.auth_header_name.as_bytes())
                .map_err(|e| ToolSetsError::InvalidHeader(e.to_string()))?,
            HeaderValue::from_str(header_value)
                .map_err(|e| ToolSetsError::InvalidHeader(e.to_string()))?,
        );
    }
    let transport_config = StreamableHttpClientTransportConfig::with_uri(upstream.url.as_str())
        .custom_headers(headers);
    let worker = StreamableHttpClientWorker::new(reqwest::Client::new(), transport_config);
    let client = ().serve(worker).await.map_err(Box::new)?;
    Ok(client)
}

async fn refresh_loop(
    upstream: McpUpstreamConfig,
    provider: Arc<GitHubAppTokenProvider>,
    client: Arc<RwLock<RunningService<RoleClient, ()>>>,
) {
    loop {
        tokio::time::sleep(GITHUB_APP_REFRESH_INTERVAL).await;
        let token = match provider.generate_token().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    upstream = %upstream.name,
                    error = %e,
                    "github_app token refresh failed; will retry on next cycle"
                );
                continue;
            }
        };
        let header_value = format!("Bearer {}", token.token);
        match build_rmcp_client(&upstream, &header_value).await {
            Ok(new_client) => {
                *client.write().await = new_client;
                tracing::info!(
                    upstream = %upstream.name,
                    "rebuilt mcp upstream client with refreshed github_app token"
                );
            }
            Err(e) => tracing::warn!(
                upstream = %upstream.name,
                error = %e,
                "failed to rebuild rmcp client after token refresh"
            ),
        }
    }
}

#[async_trait::async_trait]
impl SearchableToolSet for UpstreamToolSet {
    fn name(&self) -> &str {
        &self.name
    }

    fn prefix(&self) -> &str {
        &self.tool_prefix
    }

    fn category(&self) -> &str {
        &self.category
    }

    fn category_description(&self) -> &str {
        &self.category_description
    }

    fn tools(&self) -> &[ToolSetEntry] {
        &self.tools
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        has_required_scopes(&self.required_scopes, subject)
            && (!self.internal_only || subject.is_agent())
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let mut params = CallToolRequestParams::new(tool_name.to_string());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }
        let client = self.client.read().await;
        let result = client.peer().call_tool(params).await?;
        Ok(normalize_upstream_result(&self.name, tool_name, result))
    }
}

fn has_required_scopes(required: &[AuthScope], subject: &AuthSubject) -> bool {
    required.iter().all(|scope| subject.has_scope(scope))
}

fn normalize_upstream_result(
    upstream_name: &str,
    tool_name: &str,
    result: CallToolResult,
) -> CallToolResult {
    if result.is_error == Some(true) || looks_like_textual_upstream_error(&result) {
        let message = extract_text(&result);
        let mut out = CallToolResult::error(result.content);
        out.structured_content = Some(serde_json::json!({
            "ok": false,
            "error": {
                "upstream": upstream_name,
                "tool": tool_name,
                "message": message,
            }
        }));
        return out;
    }
    result
}

fn looks_like_textual_upstream_error(result: &CallToolResult) -> bool {
    if result.structured_content.is_some() || result.content.len() != 1 {
        return false;
    }
    let text = extract_text(result);
    let lower = text.to_ascii_lowercase();
    lower.starts_with("failed to ")
        || lower.starts_with("error: ")
        || lower.contains(" resource not accessible by integration")
}

fn extract_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::{AgentId, McpCredsId, ProjectId, UserId};
    use rmcp::model::Content;

    fn user_subject() -> AuthSubject {
        AuthSubject::User(UserId::new())
    }

    fn agent_subject() -> AuthSubject {
        AuthSubject::Agent(ProjectId::new(), AgentId::new(), Vec::new())
    }

    fn exported_agent_subject() -> AuthSubject {
        AuthSubject::ExportedAgent(UserId::new(), McpCredsId::new(), Vec::new())
    }

    fn visible(internal_only: bool, subject: &AuthSubject) -> bool {
        // Mirrors UpstreamToolSet::is_visible without needing a full
        // RunningService — exercises the internal_only/required_scopes gate.
        has_required_scopes(&[], subject) && (!internal_only || subject.is_agent())
    }

    #[test]
    fn internal_only_hides_from_user() {
        assert!(!visible(true, &user_subject()));
    }

    #[test]
    fn internal_only_hides_from_exported_agent() {
        assert!(!visible(true, &exported_agent_subject()));
    }

    #[test]
    fn internal_only_hides_from_anonymous() {
        assert!(!visible(true, &AuthSubject::Anonymous));
    }

    #[test]
    fn internal_only_visible_to_agent() {
        assert!(visible(true, &agent_subject()));
    }

    #[test]
    fn internal_only_unset_visible_to_user() {
        assert!(visible(false, &user_subject()));
    }

    #[test]
    fn textual_upstream_failure_becomes_structured_tool_error() {
        let result = CallToolResult::success(vec![Content::text(
            "failed to list workflow runs: 403 Resource not accessible by integration",
        )]);

        let normalized = normalize_upstream_result("github_actions", "actions_list", result);

        assert_eq!(normalized.is_error, Some(true));
        let structured = normalized
            .structured_content
            .expect("normalized errors carry structured metadata");
        assert_eq!(
            structured["error"]["upstream"],
            serde_json::json!("github_actions")
        );
        assert_eq!(
            structured["error"]["tool"],
            serde_json::json!("actions_list")
        );
    }

    #[test]
    fn structured_success_is_not_reclassified_by_text() {
        let mut result = CallToolResult::success(vec![Content::text("failed to parse: example")]);
        result.structured_content = Some(serde_json::json!({"value": "failed to parse: example"}));

        let normalized = normalize_upstream_result("demo", "tool", result);

        assert_ne!(normalized.is_error, Some(true));
    }
}
