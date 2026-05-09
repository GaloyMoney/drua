use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use github_app::GitHubAppTokenProvider;
use http::{HeaderName, HeaderValue};
use rmcp::{
    model::{CallToolRequestParams, CallToolResult, JsonObject},
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
        let mut result = client.peer().call_tool(params).await?;
        reify_json_structured_content(&mut result);
        Ok(result)
    }
}

/// When an upstream MCP tool returns its result as JSON-encoded text in
/// `content[].text` without populating `structured_content`, parse it
/// and stash the parsed value on `structured_content` so the classifier
/// walker can do tree-aware elision instead of falling back to byte
/// head/tail. No-op when the tool already provided structured content,
/// when the text isn't JSON-shaped, or when parsing yields a scalar
/// (only `Object`/`Array` are worth reifying).
fn reify_json_structured_content(result: &mut CallToolResult) {
    if result.structured_content.is_some() {
        return;
    }
    let mut combined = String::new();
    for part in &result.content {
        if let rmcp::model::RawContent::Text(t) = &part.raw {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&t.text);
        }
    }
    let trimmed = combined.trim_start();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return;
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) => {
            result.structured_content = Some(v);
        }
        _ => {}
    }
}

fn has_required_scopes(required: &[AuthScope], subject: &AuthSubject) -> bool {
    required.iter().all(|scope| subject.has_scope(scope))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::{AgentId, McpCredsId, ProjectId, UserId};

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

    use rmcp::model::Content;

    #[test]
    fn reify_object_text_into_structured_content() {
        let mut r = CallToolResult::success(vec![Content::text(
            r#"{"total":2,"items":[{"id":1},{"id":2}]}"#.to_string(),
        )]);
        reify_json_structured_content(&mut r);
        let sc = r.structured_content.expect("structured_content set");
        assert_eq!(sc.get("total"), Some(&serde_json::json!(2)));
        assert!(sc.get("items").unwrap().is_array());
    }

    #[test]
    fn reify_array_text_into_structured_content() {
        let mut r = CallToolResult::success(vec![Content::text(
            r#"[{"a":1},{"a":2},{"a":3}]"#.to_string(),
        )]);
        reify_json_structured_content(&mut r);
        let sc = r.structured_content.expect("structured_content set");
        assert!(sc.is_array());
        assert_eq!(sc.as_array().unwrap().len(), 3);
    }

    #[test]
    fn reify_skips_when_structured_content_already_set() {
        let mut r = CallToolResult::success(vec![Content::text(r#"{"a":1}"#.to_string())]);
        r.structured_content = Some(serde_json::json!({"existing": true}));
        reify_json_structured_content(&mut r);
        assert_eq!(
            r.structured_content,
            Some(serde_json::json!({"existing": true}))
        );
    }

    #[test]
    fn reify_skips_non_json_text() {
        let mut r = CallToolResult::success(vec![Content::text(
            "NAME    READY   STATUS\nfoo     1/1     Running\n".to_string(),
        )]);
        reify_json_structured_content(&mut r);
        assert!(r.structured_content.is_none());
    }

    #[test]
    fn reify_skips_scalar_json() {
        let mut r = CallToolResult::success(vec![Content::text("42".to_string())]);
        reify_json_structured_content(&mut r);
        assert!(r.structured_content.is_none());

        let mut r = CallToolResult::success(vec![Content::text(r#""just a string""#.to_string())]);
        reify_json_structured_content(&mut r);
        assert!(r.structured_content.is_none());
    }

    #[test]
    fn reify_skips_truncated_or_invalid_json() {
        let mut r = CallToolResult::success(vec![Content::text(r#"{"a": 1, "b":"#.to_string())]);
        reify_json_structured_content(&mut r);
        assert!(r.structured_content.is_none());
    }

    #[test]
    fn reify_handles_leading_whitespace() {
        let mut r =
            CallToolResult::success(vec![Content::text("  \n\t  [1,2,3]\n".to_string())]);
        reify_json_structured_content(&mut r);
        assert!(r.structured_content.unwrap().is_array());
    }
}
