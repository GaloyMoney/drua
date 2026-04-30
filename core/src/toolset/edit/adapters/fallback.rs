//! Fallback adapter — single mega-tool `edit_file` for tool-schema-agnostic
//! models (Llama, DeepSeek, Grok, OpenRouter passthrough). Mirrors Anthropic's
//! canonical four-command shape but is declared as a regular function tool
//! with a thorough description and inline examples.

use serde_json::json;

use super::super::adapter::{ProviderEditAdapter, ToolDeclaration};
use super::super::canonical::{EditExecError, EditOp, EditOpResult, EditParseError};

pub const EDIT_FILE: &str = "edit_file";

#[derive(Debug, Clone, Default)]
pub struct FallbackAdapter;

impl FallbackAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl ProviderEditAdapter for FallbackAdapter {
    fn provider_name(&self) -> &'static str {
        "fallback"
    }

    fn declare_tools(&self) -> Vec<ToolDeclaration> {
        let description = "View, create, edit, or insert into files.\n\
            Choose `command` based on need:\n\
            - view: read file or list directory; returns numbered lines\n\
            - create: create new file with `file_text`\n\
            - str_replace: replace exactly-once-matching `old_str` with \
              `new_str` (include 3+ lines of context to ensure uniqueness)\n\
            - insert: insert `new_str` after `insert_line` (0 = start of file)\n\
            \n\
            Example calls:\n\
            {\"command\": \"view\", \"path\": \"src/lib.rs\"}\n\
            {\"command\": \"str_replace\", \"path\": \"src/lib.rs\", \
            \"old_str\": \"    let x = 1;\", \
            \"new_str\": \"    let x = 10;\"}";

        vec![ToolDeclaration {
            name: EDIT_FILE.to_string(),
            description: description.to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command":     {"type": "string", "enum": ["view", "create", "str_replace", "insert"]},
                    "path":        {"type": "string"},
                    "view_range":  {
                        "type": "array",
                        "items": {"type": "integer"},
                        "minItems": 2,
                        "maxItems": 2
                    },
                    "old_str":     {"type": "string"},
                    "new_str":     {"type": "string"},
                    "file_text":   {"type": "string"},
                    "insert_line": {"type": "integer", "minimum": 0}
                },
                "required": ["command", "path"],
                "additionalProperties": false
            }),
            provider_type: None,
        }]
    }

    fn parse_call(
        &self,
        tool_name: &str,
        raw_input: &serde_json::Value,
    ) -> Result<Vec<EditOp>, EditParseError> {
        if tool_name != EDIT_FILE {
            return Err(EditParseError::UnknownCommand(tool_name.to_string()));
        }
        let op: EditOp = serde_json::from_value(raw_input.clone())
            .map_err(|e| EditParseError::InvalidShape(e.to_string()))?;
        Ok(vec![op])
    }

    fn format_result(
        &self,
        tool_use_id: &str,
        results: &[EditOpResult],
        errors: &[EditExecError],
    ) -> serde_json::Value {
        let is_error = !errors.is_empty();
        let text = if is_error {
            errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            results
                .iter()
                .map(EditOpResult::human_summary)
                .collect::<Vec<_>>()
                .join("\n")
        };
        json!({
            "tool_call_id": tool_use_id,
            "is_error": is_error,
            "output": text,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_single_tool() {
        let adapter = FallbackAdapter::new();
        let decl = adapter.declare_tools();
        assert_eq!(decl.len(), 1);
        assert_eq!(decl[0].name, "edit_file");
    }

    #[test]
    fn parse_view_round_trip() {
        let adapter = FallbackAdapter::new();
        let raw = json!({"command": "view", "path": "/w/a.rs"});
        let ops = adapter.parse_call(EDIT_FILE, &raw).unwrap();
        assert!(matches!(&ops[0], EditOp::View { .. }));
    }

    #[test]
    fn parse_unknown_tool_errors() {
        let adapter = FallbackAdapter::new();
        assert!(adapter.parse_call("apply_patch", &json!({})).is_err());
    }
}
