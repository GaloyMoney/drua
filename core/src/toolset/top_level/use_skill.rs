use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::auth::AuthSubject;
use crate::library::Library;
use crate::skill::Skills;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::parse_params;

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

fn default_search_limit() -> usize {
    10
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum UseSkillParams {
    Invoke {
        name: String,
        #[serde(default)]
        arguments: Option<String>,
    },
    Search {
        query: String,
        #[serde(default = "default_search_limit")]
        limit: usize,
    },
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

// ---------------------------------------------------------------------------
// $ARGUMENTS substitution
// ---------------------------------------------------------------------------

/// Substitute `$ARGUMENTS` in the skill body with the provided arguments.
/// If the body doesn't contain `$ARGUMENTS` and arguments is non-empty,
/// appends `ARGUMENTS: <value>` to the end.
fn substitute_arguments(body: &str, arguments: Option<&str>) -> String {
    let args = arguments.unwrap_or_default();
    if body.contains("$ARGUMENTS") {
        body.replace("$ARGUMENTS", args)
    } else if !args.is_empty() {
        format!("{body}\n\nARGUMENTS: {args}")
    } else {
        body.to_string()
    }
}

// ---------------------------------------------------------------------------
// UseSkillTool
// ---------------------------------------------------------------------------

pub struct UseSkillTool {
    skills: Arc<Skills>,
    library: Library,
}

impl UseSkillTool {
    pub fn new(skills: Arc<Skills>, library: Library) -> Self {
        Self { skills, library }
    }
}

#[async_trait::async_trait]
impl TopLevelTool for UseSkillTool {
    fn name(&self) -> &str {
        "use_skill"
    }

    fn description(&self) -> &str {
        "Invoke or search workspace skills. Use action \"invoke\" with a skill \
         name to load the full skill body (with optional $ARGUMENTS substitution). \
         Use action \"search\" with a query to discover skills by topic."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &USE_SKILL_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.workspace_id().is_some()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: UseSkillParams = parse_params(arguments)?;

        match params {
            UseSkillParams::Invoke { name, arguments } => {
                let sandbox_id = subject.readable_sandbox_id();
                let body = self
                    .skills
                    .find_by_name(&name, sandbox_id)
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;

                match body {
                    Some(body) => {
                        let rendered = substitute_arguments(&body, arguments.as_deref());
                        Ok(CallToolResult::success(vec![Content::text(rendered)]))
                    }
                    None => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Unknown skill: {name}"
                    ))])),
                }
            }

            UseSkillParams::Search { query, limit } => {
                let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
                let results = self
                    .library
                    .search(
                        uuid::Uuid::from(workspace_id),
                        &query,
                        Some(crate::library::DocType::Skill),
                        limit,
                    )
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;

                if results.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No skills found matching your query.",
                    )]));
                }

                let text = results
                    .iter()
                    .map(|r| format!("- {} — {}", r.title, truncate(&r.content, 200)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_replaces_arguments_placeholder() {
        let body = "Deploy $ARGUMENTS to production.";
        let result = substitute_arguments(body, Some("staging"));
        assert_eq!(result, "Deploy staging to production.");
    }

    #[test]
    fn substitute_appends_when_no_placeholder() {
        let body = "Run the deploy process.";
        let result = substitute_arguments(body, Some("staging"));
        assert_eq!(result, "Run the deploy process.\n\nARGUMENTS: staging");
    }

    #[test]
    fn substitute_noop_when_no_args_and_no_placeholder() {
        let body = "Run the deploy process.";
        let result = substitute_arguments(body, None);
        assert_eq!(result, "Run the deploy process.");
    }

    #[test]
    fn substitute_replaces_multiple_occurrences() {
        let body = "First: $ARGUMENTS, second: $ARGUMENTS";
        let result = substitute_arguments(body, Some("val"));
        assert_eq!(result, "First: val, second: val");
    }
}
