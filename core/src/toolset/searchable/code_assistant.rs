use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};

use crate::auth::AuthSubject;
use crate::code_assistant::{CodeAssistant, SearchCodeParams};

use super::super::{SearchableToolSet, ToolSetEntry, ToolSetsError};

fn parse_params<T: serde::de::DeserializeOwned>(
    arguments: Option<JsonObject>,
) -> Result<T, ToolSetsError> {
    let value = serde_json::Value::Object(arguments.unwrap_or_default());
    serde_json::from_value(value).map_err(|e| ToolSetsError::InvalidArgument(e.to_string()))
}

fn schema_for<T: schemars::JsonSchema>() -> serde_json::Value {
    let settings = schemars::gen::SchemaSettings::draft07().with(|s| {
        s.inline_subschemas = true;
        s.meta_schema = None;
    });
    let generator = settings.into_generator();
    let schema = generator.into_root_schema_for::<T>();
    let mut value = serde_json::to_value(schema).expect("schema serialization");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("title");
        // `definitions` retained for $ref resolution by the compose TS generator.
        obj.insert(
            "additionalProperties".into(),
            serde_json::Value::Bool(false),
        );
    }
    value
}

static SEARCH_CODE_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<SearchCodeParams>);

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
        tool.input_schema =
            Arc::new(serde_json::from_value((*SEARCH_CODE_SCHEMA).clone()).unwrap_or_default());

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
        _subject: &AuthSubject,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        match tool_name {
            "search_code" => {
                let params: SearchCodeParams = parse_params(arguments)?;
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
