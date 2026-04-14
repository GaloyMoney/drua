//! `text_editor` — view, create, and edit files in the sandbox environment.

use std::sync::LazyLock;

use rmcp::model::{CallToolResult, Content, JsonObject};

use crate::auth::AuthSubject;

use super::super::super::error::ToolSetsError;
use super::super::super::traits::TopLevelTool;
use super::SandboxClient;

pub struct SandboxTextEditor {
    client: SandboxClient,
}

impl SandboxTextEditor {
    pub fn new(sandbox_url: String) -> Self {
        Self {
            client: SandboxClient::new(sandbox_url),
        }
    }
}

static TEXT_EDITOR_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The editor command: view, create, str_replace, or insert.",
                "enum": ["view", "create", "str_replace", "insert"]
            },
            "path": {
                "type": "string",
                "description": "Absolute path to the file or directory."
            },
            "file_text": {
                "type": "string",
                "description": "Content for the 'create' command."
            },
            "old_str": {
                "type": "string",
                "description": "Text to find for 'str_replace' (must match exactly once)."
            },
            "new_str": {
                "type": "string",
                "description": "Replacement text for 'str_replace' or text to insert for 'insert'."
            },
            "insert_line": {
                "type": "integer",
                "description": "Line number after which to insert text (0 = beginning of file)."
            },
            "view_range": {
                "type": "array",
                "items": { "type": "integer" },
                "description": "Optional [start, end] line range for 'view' (-1 = end of file)."
            }
        },
        "required": ["command", "path"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for SandboxTextEditor {
    fn name(&self) -> &str {
        "text_editor"
    }

    fn description(&self) -> &str {
        "View, create, and edit files in the sandbox. Commands: \
         view (read file or list directory), create (write new file), \
         str_replace (find-and-replace exactly one match), \
         insert (add text after a line number)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &TEXT_EDITOR_SCHEMA
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let input = arguments
            .map(serde_json::Value::Object)
            .unwrap_or(serde_json::Value::Object(Default::default()));

        let (output, is_error) = self
            .client
            .execute("str_replace_based_edit_tool", &input)
            .await?;

        let mut result = CallToolResult::success(vec![Content::text(output)]);
        result.is_error = Some(is_error);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_editor_schema_is_valid_json_schema() {
        let tool = SandboxTextEditor::new("http://unused".into());
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["command"].is_object());
        assert!(schema["properties"]["path"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("command")));
        assert!(required.contains(&serde_json::json!("path")));
    }

    #[test]
    fn text_editor_name_and_description() {
        let tool = SandboxTextEditor::new("http://unused".into());
        assert_eq!(tool.name(), "text_editor");
        assert!(!tool.description().is_empty());
    }
}
