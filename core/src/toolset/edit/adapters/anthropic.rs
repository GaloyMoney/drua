//! Anthropic adapter — identity passthrough on the canonical [`EditOp`].
//!
//! Anthropic ships `text_editor_*` as a server tool: the model already knows
//! the schema, so the request payload only needs `{"type": "...", "name": "..."}`.
//! Concretely we still emit a JSON-Schema in [`ToolDeclaration::input_schema`]
//! for adapters that fall back to a regular function-tool envelope (e.g. when
//! the provider does not understand the server-tool shorthand).

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::super::adapter::{ProviderEditAdapter, ToolDeclaration};
use super::super::canonical::{EditExecError, EditOp, EditOpResult, EditParseError};

/// Per-version constants from the official Anthropic text-editor docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnthropicTextEditorVersion {
    /// Claude Sonnet 3.5 — original release.
    V20241022,
    /// Claude Sonnet 3.7 — same capabilities, renamed `type`.
    V20250124,
    /// Claude 4.x — `undo_edit` removed, tool renamed.
    V20250429,
    /// Claude 4.x — adds optional `max_characters`. Default.
    V20250728,
}

impl AnthropicTextEditorVersion {
    pub fn provider_type(self) -> &'static str {
        match self {
            Self::V20241022 => "text_editor_20241022",
            Self::V20250124 => "text_editor_20250124",
            Self::V20250429 => "text_editor_20250429",
            Self::V20250728 => "text_editor_20250728",
        }
    }

    pub fn tool_name(self) -> &'static str {
        match self {
            Self::V20241022 | Self::V20250124 => "str_replace_editor",
            Self::V20250429 | Self::V20250728 => "str_replace_based_edit_tool",
        }
    }

    pub fn supports_undo_edit(self) -> bool {
        matches!(self, Self::V20241022 | Self::V20250124)
    }
}

#[derive(Debug, Clone)]
pub struct AnthropicAdapter {
    version: AnthropicTextEditorVersion,
}

impl AnthropicAdapter {
    pub fn new(version: AnthropicTextEditorVersion) -> Self {
        Self { version }
    }

    pub fn latest() -> Self {
        Self::new(AnthropicTextEditorVersion::V20250728)
    }
}

impl ProviderEditAdapter for AnthropicAdapter {
    fn provider_name(&self) -> &'static str {
        "anthropic"
    }

    fn declare_tools(&self) -> Vec<ToolDeclaration> {
        vec![ToolDeclaration {
            name: self.version.tool_name().to_string(),
            description: "Anthropic-compatible text editor. Commands: \
                view, create, str_replace, insert."
                .to_string(),
            input_schema: edit_input_schema(self.version),
            provider_type: Some(self.version.provider_type().to_string()),
        }]
    }

    fn parse_call(
        &self,
        tool_name: &str,
        raw_input: &serde_json::Value,
    ) -> Result<Vec<EditOp>, EditParseError> {
        if tool_name != self.version.tool_name() {
            return Err(EditParseError::UnknownCommand(format!(
                "expected {}, got {tool_name}",
                self.version.tool_name()
            )));
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
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "is_error": is_error,
            "content": [{"type": "text", "text": text}],
        })
    }
}

/// Schema for the canonical edit tool — union of the four core commands plus
/// `view_range` / `file_text` / `old_str` / `new_str` / `insert_line`. Used
/// by the [`AnthropicAdapter`] (when a fallback function-tool envelope is
/// required) and re-exported for the existing MCP-side tool registration.
pub(crate) fn edit_input_schema(version: AnthropicTextEditorVersion) -> serde_json::Value {
    let mut commands = vec!["view", "create", "str_replace", "insert"];
    if version.supports_undo_edit() {
        commands.push("undo_edit");
    }
    json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "enum": commands,
                "description": "Which editor operation to perform."
            },
            "path": {
                "type": "string",
                "description": "Absolute path to the file or directory inside the sandbox workspace."
            },
            "view_range": {
                "type": "array",
                "items": { "type": "integer" },
                "minItems": 2,
                "maxItems": 2,
                "description": "[start, end] line range for `view`. -1 for end means EOF. Optional."
            },
            "file_text": {
                "type": "string",
                "description": "Full file contents for `create`."
            },
            "old_str": {
                "type": "string",
                "description": "Substring to replace for `str_replace`. Must appear exactly once."
            },
            "new_str": {
                "type": "string",
                "description": "Replacement text for `str_replace`, or text to insert for `insert`."
            },
            "insert_line": {
                "type": "integer",
                "minimum": 0,
                "description": "Line number after which to insert text for `insert`. 0 = beginning of file."
            }
        },
        "required": ["command", "path"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_envelopes_match_docs() {
        assert_eq!(
            AnthropicTextEditorVersion::V20241022.provider_type(),
            "text_editor_20241022"
        );
        assert_eq!(
            AnthropicTextEditorVersion::V20241022.tool_name(),
            "str_replace_editor"
        );
        assert_eq!(
            AnthropicTextEditorVersion::V20250728.provider_type(),
            "text_editor_20250728"
        );
        assert_eq!(
            AnthropicTextEditorVersion::V20250728.tool_name(),
            "str_replace_based_edit_tool"
        );
    }

    #[test]
    fn undo_edit_only_on_legacy_versions() {
        assert!(AnthropicTextEditorVersion::V20241022.supports_undo_edit());
        assert!(AnthropicTextEditorVersion::V20250124.supports_undo_edit());
        assert!(!AnthropicTextEditorVersion::V20250429.supports_undo_edit());
        assert!(!AnthropicTextEditorVersion::V20250728.supports_undo_edit());
    }

    #[test]
    fn declare_tools_emits_provider_type() {
        let adapter = AnthropicAdapter::latest();
        let decl = adapter.declare_tools();
        assert_eq!(decl.len(), 1);
        assert_eq!(decl[0].name, "str_replace_based_edit_tool");
        assert_eq!(
            decl[0].provider_type.as_deref(),
            Some("text_editor_20250728")
        );
    }

    #[test]
    fn parse_call_round_trip_str_replace() {
        let adapter = AnthropicAdapter::latest();
        let raw = serde_json::json!({
            "command": "str_replace",
            "path": "/w/a.rs",
            "old_str": "let x = 1;",
            "new_str": "let x = 2;",
        });
        let ops = adapter
            .parse_call("str_replace_based_edit_tool", &raw)
            .unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            EditOp::StrReplace {
                path,
                old_str,
                new_str,
            } => {
                assert_eq!(path, "/w/a.rs");
                assert_eq!(old_str, "let x = 1;");
                assert_eq!(new_str, "let x = 2;");
            }
            _ => panic!("expected str_replace"),
        }
    }

    #[test]
    fn parse_call_rejects_wrong_tool_name() {
        let adapter = AnthropicAdapter::latest();
        let raw = serde_json::json!({"command": "view", "path": "/w/a.rs"});
        assert!(adapter.parse_call("apply_patch", &raw).is_err());
    }

    #[test]
    fn format_result_emits_anthropic_envelope() {
        let adapter = AnthropicAdapter::latest();
        let v = adapter.format_result(
            "toolu_01",
            &[EditOpResult::Replaced {
                path: "/w/a.rs".to_string(),
                occurrences: 1,
            }],
            &[],
        );
        assert_eq!(v["type"], "tool_result");
        assert_eq!(v["tool_use_id"], "toolu_01");
        assert_eq!(v["is_error"], false);
    }

    #[test]
    fn format_result_marks_errors() {
        let adapter = AnthropicAdapter::latest();
        let v = adapter.format_result(
            "toolu_01",
            &[],
            &[EditExecError::NonUniqueMatch {
                path: "/w/a.rs".to_string(),
                actual: 3,
            }],
        );
        assert_eq!(v["is_error"], true);
    }
}
