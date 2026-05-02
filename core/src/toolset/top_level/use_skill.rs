use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::skill::Skills;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;

fn default_search_limit() -> usize {
    10
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum UseSkillAction {
    Invoke {
        name: String,
        #[serde(default)]
        arguments: Option<String>,
    },
    Search {
        #[serde(default)]
        query: String,
        #[serde(default = "default_search_limit")]
        limit: usize,
    },
}

/// Wrapper that defaults to Search when `action` is missing.
fn parse_use_skill_params(arguments: Option<JsonObject>) -> Result<UseSkillAction, ToolSetsError> {
    let Some(args) = arguments else {
        return Ok(UseSkillAction::Search {
            query: String::new(),
            limit: default_search_limit(),
        });
    };
    let map: serde_json::Map<String, serde_json::Value> = args.into_iter().collect();
    if map.contains_key("action") {
        let val = serde_json::Value::Object(map);
        serde_json::from_value(val).map_err(|e| ToolSetsError::Skill(e.to_string()))
    } else if let Some(name) = map.get("name").and_then(|v| v.as_str()) {
        Ok(UseSkillAction::Invoke {
            name: name.to_string(),
            arguments: map
                .get("arguments")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    } else {
        Ok(UseSkillAction::Search {
            query: map
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            limit: map
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(default_search_limit() as u64) as usize,
        })
    }
}

static USE_SKILL_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["invoke", "search"],
                "description": "Which operation to perform: invoke a skill by name, or search for skills by topic."
            },
            "name": {
                "type": "string",
                "description": "Skill name to invoke (invoke action)."
            },
            "arguments": {
                "type": "string",
                "description": "Arguments to pass to the skill. Replaces $ARGUMENTS in the skill body (invoke action, optional)."
            },
            "query": {
                "type": "string",
                "description": "Search query — keywords or natural language (search action)."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "description": "Maximum number of search results (search action, default 10)."
            }
        },
        "required": ["action"],
        "additionalProperties": false
    })
});

pub struct UseSkillTool {
    skills: Arc<Skills>,
}

impl UseSkillTool {
    pub fn new(skills: Arc<Skills>) -> Self {
        Self { skills }
    }
}

#[async_trait::async_trait]
impl TopLevelTool for UseSkillTool {
    fn name(&self) -> &str {
        "use_skill"
    }

    fn description(&self) -> &str {
        "Invoke or search project skills. Use action \"invoke\" with a skill \
         name to load the full skill body (with optional $ARGUMENTS substitution). \
         Use action \"search\" with a query to discover skills by topic."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &USE_SKILL_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.project_id().is_some_and(|project| {
            subject
                .can(AuthVerb::Use, AuthResource::Skill(project, None))
                .is_ok()
        })
    }

    fn composable(&self) -> bool {
        false
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params = parse_use_skill_params(arguments)?;

        match params {
            UseSkillAction::Invoke { name, arguments } => {
                let project_id = subject.project_id();
                let sandbox_id = subject.readable_sandbox_id();
                let rendered = self
                    .skills
                    .interpolate_skill(&name, project_id, sandbox_id, arguments.as_deref())
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;

                match rendered {
                    Some(body) => Ok(CallToolResult::success(vec![Content::text(body)])),
                    None => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Unknown skill: {name}"
                    ))])),
                }
            }

            UseSkillAction::Search { query, limit } => {
                let project_id = subject.project_id().ok_or(ToolSetsError::Unauthorized)?;
                let results = self
                    .skills
                    .search(subject, project_id, &query, limit)
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;

                if results.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No skills found matching your query.",
                    )]));
                }

                let text = results
                    .iter()
                    .map(|r| format!("- {} — {}", r.fields.name, truncate(&r.fields.content, 200)))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Found {} skill(s):\n\n{text}",
                    results.len(),
                ))]))
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}...")
    }
}
