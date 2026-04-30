//! Gemini adapter — three separate tools matching the Gemini CLI muscle
//! memory: `replace`, `write_file`, `read_file`. No `insert` analog.
//!
//! `replace` defaults to `expected_replacements = 1`. Setting it > 1 is
//! reflected in the canonical [`EditOp::StrReplace`] only insofar as the
//! executor is expected to refuse if the actual count differs (uniqueness
//! invariant matches Anthropic's `str_replace`).

use serde_json::json;

use super::super::adapter::{ProviderEditAdapter, ToolDeclaration};
use super::super::canonical::{EditExecError, EditOp, EditOpResult, EditParseError};

pub const REPLACE: &str = "replace";
pub const WRITE_FILE: &str = "write_file";
pub const READ_FILE: &str = "read_file";

#[derive(Debug, Clone, Default)]
pub struct GeminiAdapter;

impl GeminiAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl ProviderEditAdapter for GeminiAdapter {
    fn provider_name(&self) -> &'static str {
        "gemini"
    }

    fn declare_tools(&self) -> Vec<ToolDeclaration> {
        vec![
            ToolDeclaration {
                name: REPLACE.to_string(),
                description: "Replace exactly one occurrence of `old_string` with \
                     `new_string` in `file_path`. `old_string` must be unique \
                     in the file (include 3+ lines of surrounding context to \
                     ensure uniqueness). Set `expected_replacements > 1` to \
                     replace multiple matches."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "file_path":  {"type": "string"},
                        "old_string": {"type": "string"},
                        "new_string": {"type": "string"},
                        "expected_replacements": {"type": "integer", "default": 1, "minimum": 1}
                    },
                    "required": ["file_path", "old_string", "new_string"],
                    "additionalProperties": false
                }),
                provider_type: None,
            },
            ToolDeclaration {
                name: WRITE_FILE.to_string(),
                description: "Create or completely overwrite a file with the \
                              given content."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string"},
                        "content":   {"type": "string"}
                    },
                    "required": ["file_path", "content"],
                    "additionalProperties": false
                }),
                provider_type: None,
            },
            ToolDeclaration {
                name: READ_FILE.to_string(),
                description: "Read a file with 1-indexed line numbers \
                              prepended. Optional [start, end] line range; \
                              -1 for EOF."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "view_range": {
                            "type": "array",
                            "items": {"type": "integer"},
                            "minItems": 2,
                            "maxItems": 2
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
                provider_type: None,
            },
        ]
    }

    fn parse_call(
        &self,
        tool_name: &str,
        raw_input: &serde_json::Value,
    ) -> Result<Vec<EditOp>, EditParseError> {
        match tool_name {
            REPLACE => {
                let path = req_str(raw_input, "file_path")?;
                let old_str = req_str(raw_input, "old_string")?;
                let new_str = req_str(raw_input, "new_string")?;
                Ok(vec![EditOp::StrReplace {
                    path,
                    old_str,
                    new_str,
                }])
            }
            WRITE_FILE => {
                let path = req_str(raw_input, "file_path")?;
                let file_text = req_str(raw_input, "content")?;
                Ok(vec![EditOp::Create { path, file_text }])
            }
            READ_FILE => {
                let path = req_str(raw_input, "path")?;
                let view_range = raw_input
                    .get("view_range")
                    .map(|v| {
                        serde_json::from_value::<[i64; 2]>(v.clone())
                            .map_err(|e| EditParseError::InvalidShape(format!("view_range: {e}")))
                    })
                    .transpose()?;
                Ok(vec![EditOp::View { path, view_range }])
            }
            other => Err(EditParseError::UnknownCommand(other.to_string())),
        }
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

fn req_str(raw: &serde_json::Value, field: &'static str) -> Result<String, EditParseError> {
    raw.get(field)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or(EditParseError::MissingField(field))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_three_tools() {
        let adapter = GeminiAdapter::new();
        let names: Vec<_> = adapter
            .declare_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["replace", "write_file", "read_file"]);
    }

    #[test]
    fn parse_replace() {
        let adapter = GeminiAdapter::new();
        let raw = json!({
            "file_path": "/w/a.rs",
            "old_string": "let x = 1;",
            "new_string": "let x = 2;",
        });
        let ops = adapter.parse_call(REPLACE, &raw).unwrap();
        assert!(matches!(&ops[0], EditOp::StrReplace { path, .. } if path == "/w/a.rs"));
    }

    #[test]
    fn parse_write_file() {
        let adapter = GeminiAdapter::new();
        let raw = json!({"file_path": "x", "content": "y"});
        let ops = adapter.parse_call(WRITE_FILE, &raw).unwrap();
        match &ops[0] {
            EditOp::Create { path, file_text } => {
                assert_eq!(path, "x");
                assert_eq!(file_text, "y");
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn parse_read_file_without_range() {
        let adapter = GeminiAdapter::new();
        let raw = json!({"path": "x"});
        let ops = adapter.parse_call(READ_FILE, &raw).unwrap();
        assert!(matches!(
            &ops[0],
            EditOp::View {
                view_range: None,
                ..
            }
        ));
    }

    #[test]
    fn missing_field_errors() {
        let adapter = GeminiAdapter::new();
        assert!(adapter.parse_call(REPLACE, &json!({})).is_err());
    }
}
