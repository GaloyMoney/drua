use std::sync::Arc;

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};

use crate::code_assistant::{CodeAssistant, SearchCodeParams};

use super::super::{SearchableToolSet, ToolSetEntry, ToolSetsError};

pub struct CodeAssistantToolSet {
    service: Arc<CodeAssistant>,
    tools: Vec<ToolSetEntry>,
}

impl CodeAssistantToolSet {
    pub fn new(service: Arc<CodeAssistant>) -> Self {
        let mut tool = Tool::default();
        tool.name = "search_code".to_string().into();
        tool.description = Some(
            "Search indexed codebases for code patterns matching a query.\n\nUsage tips:\n\
             - Pass a code snippet as the query (e.g. the pattern you are about to write) — \
             code-as-query gives much better results than natural language\n\
             - Always pass a `label` filter for precise results\n\
             - Adopt the style, naming, and structure from the returned examples — don't \
             guess conventions, search first\n\nAvailable labels: entity, entity_event, \
             entity_command, entity_query, entity_hydration, error, service, service_method, \
             repository, domain_primitives, value_object, type_conversion, config, test, api, \
             job, event_handler, authorization, published_event, new_entity, none (unlabeled \
             chunks)\n\nAvailable filters: query (required), limit, repo, language, label"
                .to_string()
                .into(),
        );
        tool.input_schema = Arc::new(
            serde_json::from_value(serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query. Pass a code snippet (e.g. the pattern you are about to write) for best results — code-as-query gives much better similarity matches than natural language"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 5)"
                    },
                    "repo": {
                        "type": "string",
                        "description": "Filter results to a specific repository name"
                    },
                    "language": {
                        "type": "string",
                        "description": "Filter results to a specific language (e.g. 'rust', 'bats', 'bash')"
                    },
                    "label": {
                        "type": "string",
                        "description": "Filter results to a specific primary label. Values: entity, entity_command, entity_query, entity_hydration, entity_event, published_event, new_entity, service_method, service, repository, error, authorization, value_object, domain_primitives, api, job, event_handler, type_conversion, test, config, none (unlabeled chunks)"
                    }
                },
                "required": ["query"]
            }))
            .unwrap_or_default(),
        );

        let tools = vec![ToolSetEntry {
            name: "search_code".to_string(),
            description: tool,
            default_output_filter: None,
        }];

        Self { service, tools }
    }
}

#[async_trait::async_trait]
impl SearchableToolSet for CodeAssistantToolSet {
    fn name(&self) -> &str {
        "code_assistant"
    }

    fn category(&self) -> &str {
        "code-quality"
    }

    fn category_description(&self) -> &str {
        "Code search, style review, anti-pattern detection"
    }

    fn tools(&self) -> &[ToolSetEntry] {
        &self.tools
    }

    async fn call(
        &self,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        match tool_name {
            "search_code" => {
                let params: SearchCodeParams = serde_json::from_value(serde_json::Value::Object(
                    arguments.unwrap_or_default(),
                ))
                .map_err(|e| ToolSetsError::MissingArgument(e.to_string()))?;
                let text = self
                    .service
                    .search(params)
                    .await
                    .map_err(|e| ToolSetsError::CodeAssistant(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            _ => Err(ToolSetsError::ToolNotFound(tool_name.to_string())),
        }
    }
}
