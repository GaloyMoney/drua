use std::sync::Arc;

use crate::report::{Reports, StoreReportParams};
use rmcp::model::{CallToolResult, Content, JsonObject, Tool};

use crate::auth::AuthContext;

use super::{ToolSet, ToolSetEntry, ToolSetsError};

pub struct ReportToolSet {
    service: Arc<Reports>,
    tools: Vec<ToolSetEntry>,
}

impl ReportToolSet {
    pub fn new(service: Arc<Reports>) -> Self {
        let tools = vec![
            tool_entry(
                "store_report",
                "Store a research finding, decision, or piece of knowledge for future agents. Always store important findings before completing a task.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Descriptive title for the report"
                        },
                        "content": {
                            "type": "string",
                            "description": "Full content (findings, decisions, patterns, etc.)"
                        },
                        "tags": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Lowercase tags for categorization"
                        }
                    },
                    "required": ["title", "content"]
                }),
            ),
            tool_entry(
                "search_report",
                "Search stored reports and research findings. Always search before starting research — someone may have already investigated your topic.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query (keywords or natural language)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of results to return (default: 10)"
                        }
                    },
                    "required": ["query"]
                }),
            ),
            tool_entry(
                "get_report",
                "Retrieve the full content of a stored report by its ID (or ID prefix).",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "report_id": {
                            "type": "string",
                            "description": "The report ID or prefix (e.g. first 8 characters)"
                        }
                    },
                    "required": ["report_id"]
                }),
            ),
        ];

        Self { service, tools }
    }

    async fn handle_store(&self, args: &JsonObject) -> Result<CallToolResult, ToolSetsError> {
        let title = str_arg(args, "title")?;
        let content = str_arg(args, "content")?;

        let tags: Vec<String> = args
            .get("tags")
            .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|t| t.to_lowercase())
            .collect();

        let report = self
            .service
            .store(StoreReportParams {
                title: title.to_string(),
                content: content.to_string(),
                tags: tags.clone(),
            })
            .await
            .map_err(|e| ToolSetsError::Report(e.to_string()))?;

        let id_str = report.id.to_string();
        let short_id = &id_str[..8.min(id_str.len())];
        let tags_display = if tags.is_empty() {
            String::from("(none)")
        } else {
            tags.join(", ")
        };
        let text = format!(
            "Stored report: \"{}\" (id: {short_id})\nTags: {tags_display}",
            report.title
        );
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    async fn handle_search(&self, args: &JsonObject) -> Result<CallToolResult, ToolSetsError> {
        let query = str_arg(args, "query")?;
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

        let results = self
            .service
            .search(query, limit)
            .await
            .map_err(|e| ToolSetsError::Report(e.to_string()))?;

        if results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No results found.",
            )]));
        }

        let mut text = format!("## Results ({} found)\n", results.len());

        for (i, r) in results.iter().enumerate() {
            text.push_str(&format!(
                "\n### {}. {} (score: {:.2}",
                i + 1,
                r.title,
                r.score,
            ));
            if r.pinned {
                text.push_str(", pinned");
            }
            text.push(')');

            if !r.tags.is_empty() {
                text.push_str(&format!("\nTags: {}", r.tags.join(", ")));
            }

            // Content snippet (first 300 chars).
            let snippet: String = r.content.chars().take(300).collect();
            text.push_str(&format!("\n\n{snippet}"));
            if r.content.len() > 300 {
                text.push_str("...");
            }
            text.push_str("\n\n---");
        }

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    async fn handle_get(&self, args: &JsonObject) -> Result<CallToolResult, ToolSetsError> {
        let report_id = str_arg(args, "report_id")?;

        let found = self
            .service
            .find_by_id_prefix(report_id)
            .await
            .map_err(|e| ToolSetsError::Report(e.to_string()))?;

        match found {
            Some(m) => {
                let tags_display = if m.tags.is_empty() {
                    String::from("(none)")
                } else {
                    m.tags.join(", ")
                };
                let text = format!(
                    "# {}\n\nID: {}\nTags: {tags_display}\nCreated: {}\n\n{}",
                    m.title,
                    m.id,
                    m.created_at(),
                    m.content
                );
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            None => Ok(CallToolResult::success(vec![Content::text(format!(
                "No report found with ID prefix '{report_id}'"
            ))])),
        }
    }
}

#[async_trait::async_trait]
impl ToolSet for ReportToolSet {
    fn name(&self) -> &str {
        "report"
    }

    fn category(&self) -> &str {
        "knowledge-management"
    }

    fn category_description(&self) -> &str {
        "Persistent knowledge base for research findings, decisions, and reports"
    }

    fn tools(&self) -> &[ToolSetEntry] {
        &self.tools
    }

    async fn call(
        &self,
        tool_name: &str,
        arguments: Option<JsonObject>,
        _auth: Option<&AuthContext>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let args = arguments.unwrap_or_default();
        match tool_name {
            "store_report" => self.handle_store(&args).await,
            "search_report" => self.handle_search(&args).await,
            "get_report" => self.handle_get(&args).await,
            _ => Err(ToolSetsError::ToolNotFound(tool_name.to_string())),
        }
    }
}

fn tool_entry(name: &str, description: &str, schema: serde_json::Value) -> ToolSetEntry {
    let input_schema: JsonObject = match schema {
        serde_json::Value::Object(m) => m,
        _ => Default::default(),
    };
    let mut tool = Tool::default();
    tool.name = name.to_string().into();
    tool.description = Some(description.to_string().into());
    tool.input_schema = Arc::new(input_schema);
    ToolSetEntry {
        name: name.to_string(),
        description: tool,
    }
}

fn str_arg<'a>(args: &'a JsonObject, key: &str) -> Result<&'a str, ToolSetsError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolSetsError::MissingArgument(key.to_string()))
}
