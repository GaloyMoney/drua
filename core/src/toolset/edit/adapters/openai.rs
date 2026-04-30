//! OpenAI adapter — V4A `apply_patch` envelope plus a companion `read_file`
//! tool (since `apply_patch` is write-only).
//!
//! GPT-4.1+/GPT-5/Codex were extensively trained on the V4A grammar, so this
//! is the highest-leverage non-Anthropic adapter. The adapter declares
//! tools as standard `function`-style entries; both Chat Completions and
//! Responses APIs accept that shape.

use serde_json::json;

use super::super::adapter::{ProviderEditAdapter, ToolDeclaration};
use super::super::canonical::{EditExecError, EditOp, EditOpResult, EditParseError};
use super::openai_v4a::parse_v4a_patch;

pub const APPLY_PATCH: &str = "apply_patch";
pub const READ_FILE: &str = "read_file";

#[derive(Debug, Clone, Default)]
pub struct OpenAIAdapter;

impl OpenAIAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl ProviderEditAdapter for OpenAIAdapter {
    fn provider_name(&self) -> &'static str {
        "openai"
    }

    fn declare_tools(&self) -> Vec<ToolDeclaration> {
        vec![
            ToolDeclaration {
                name: APPLY_PATCH.to_string(),
                description: "Apply a V4A patch to the working directory. Use the \
                     `*** Begin Patch` / `*** End Patch` envelope. Supports \
                     `*** Add File:`, `*** Update File:` (with `@@` hunks and \
                     `+`/`-` lines plus surrounding context), `*** Move to:`, \
                     and `*** Delete File:`."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "input": {
                            "type": "string",
                            "description": "The full V4A patch text, beginning \
                                with `*** Begin Patch` and ending with \
                                `*** End Patch`."
                        }
                    },
                    "required": ["input"],
                    "additionalProperties": false
                }),
                provider_type: None,
            },
            ToolDeclaration {
                name: READ_FILE.to_string(),
                description: "Read a file or list a directory. Returns 1-indexed \
                              numbered lines."
                    .to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "view_range": {
                            "type": "array",
                            "items": {"type": "integer"},
                            "minItems": 2,
                            "maxItems": 2,
                            "description": "Optional [start, end] line numbers. \
                                            -1 for end means EOF."
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
            APPLY_PATCH => {
                let input = raw_input
                    .get("input")
                    .and_then(|v| v.as_str())
                    .ok_or(EditParseError::MissingField("input"))?;
                parse_v4a_patch(input)
            }
            READ_FILE => {
                let path = raw_input
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or(EditParseError::MissingField("path"))?
                    .to_string();
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
        if errors.is_empty() {
            let summary = if results.len() == 1 {
                results[0].human_summary()
            } else {
                let n = results.len();
                format!(
                    "Successfully applied {n} operation(s):\n{}",
                    results
                        .iter()
                        .map(EditOpResult::human_summary)
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            };
            json!({
                "type": "apply_patch_call_output",
                "call_id": tool_use_id,
                "status": "completed",
                "output": summary,
            })
        } else {
            let detail = errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            json!({
                "type": "apply_patch_call_output",
                "call_id": tool_use_id,
                "status": "failed",
                "output": detail,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_apply_patch_and_read_file() {
        let adapter = OpenAIAdapter::new();
        let decl = adapter.declare_tools();
        assert_eq!(decl.len(), 2);
        assert_eq!(decl[0].name, "apply_patch");
        assert_eq!(decl[1].name, "read_file");
    }

    #[test]
    fn parse_apply_patch_round_trips_through_v4a() {
        let adapter = OpenAIAdapter::new();
        let raw = json!({
            "input": "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch"
        });
        let ops = adapter.parse_call(APPLY_PATCH, &raw).unwrap();
        assert!(matches!(&ops[0], EditOp::Create { path, .. } if path == "a.txt"));
    }

    #[test]
    fn parse_read_file_with_range() {
        let adapter = OpenAIAdapter::new();
        let raw = json!({"path": "/w/a.rs", "view_range": [1, -1]});
        let ops = adapter.parse_call(READ_FILE, &raw).unwrap();
        match &ops[0] {
            EditOp::View { path, view_range } => {
                assert_eq!(path, "/w/a.rs");
                assert_eq!(view_range, &Some([1, -1]));
            }
            _ => panic!("expected View"),
        }
    }

    #[test]
    fn parse_read_file_missing_path_errors() {
        let adapter = OpenAIAdapter::new();
        let raw = json!({});
        assert!(adapter.parse_call(READ_FILE, &raw).is_err());
    }

    #[test]
    fn unknown_tool_name_errors() {
        let adapter = OpenAIAdapter::new();
        assert!(adapter.parse_call("not_a_tool", &json!({})).is_err());
    }

    #[test]
    fn format_result_uses_apply_patch_call_output_envelope() {
        let adapter = OpenAIAdapter::new();
        let v = adapter.format_result(
            "call_01",
            &[EditOpResult::Created {
                path: "x".to_string(),
            }],
            &[],
        );
        assert_eq!(v["type"], "apply_patch_call_output");
        assert_eq!(v["call_id"], "call_01");
        assert_eq!(v["status"], "completed");
    }

    #[test]
    fn format_result_marks_failure() {
        let adapter = OpenAIAdapter::new();
        let v = adapter.format_result("call_01", &[], &[EditExecError::NotFound("x".to_string())]);
        assert_eq!(v["status"], "failed");
    }
}
