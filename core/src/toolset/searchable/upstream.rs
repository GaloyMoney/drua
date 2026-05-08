use std::collections::HashMap;

use http::{HeaderName, HeaderValue};
use rmcp::{
    model::{CallToolRequestParams, CallToolResult, JsonObject},
    service::RunningService,
    transport::streamable_http_client::{
        StreamableHttpClientTransportConfig, StreamableHttpClientWorker,
    },
    Peer, RoleClient, ServiceExt,
};

use crate::auth::AuthSubject;
use crate::primitives::AuthScope;

use super::super::{McpUpstreamConfig, SearchableToolSet, ToolSetEntry, ToolSetsError};

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
    client: RunningService<RoleClient, ()>,
}

impl UpstreamToolSet {
    pub(in super::super) async fn init(
        upstream: &McpUpstreamConfig,
    ) -> Result<UpstreamToolSet, ToolSetsError> {
        let mut headers = HashMap::new();
        if upstream.auth_header.is_empty() {
            if upstream.auth_required {
                let env_key = format!("{}_AUTH_HEADER", upstream.name.to_uppercase());
                return Err(ToolSetsError::MissingAuthHeader {
                    name: upstream.name.clone(),
                    env_key,
                });
            }
        } else {
            headers.insert(
                HeaderName::from_bytes(upstream.auth_header_name.as_bytes())
                    .map_err(|e| ToolSetsError::InvalidHeader(e.to_string()))?,
                HeaderValue::from_str(&upstream.auth_header)
                    .map_err(|e| ToolSetsError::InvalidHeader(e.to_string()))?,
            );
        }

        let transport_config = StreamableHttpClientTransportConfig::with_uri(upstream.url.as_str())
            .custom_headers(headers);

        let worker = StreamableHttpClientWorker::new(reqwest::Client::new(), transport_config);

        let client = ().serve(worker).await.map_err(Box::new)?;

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
                default_output_filter: None,
            })
            .collect();

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
        })
    }

    fn peer(&self) -> &Peer<RoleClient> {
        self.client.peer()
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
        let result = self.peer().call_tool(params).await?;
        Ok(result)
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
}
