use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::library::{DocType, GlobalSearchHit, Library};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

const DEFAULT_SEARCH_LIMIT: usize = 50;
const MAX_SEARCH_LIMIT: usize = 200;
const SNIPPET_CHARS: usize = 240;

fn default_search_limit() -> usize {
    DEFAULT_SEARCH_LIMIT
}

/// Workflows are git-synced to the library repo but excluded from search
/// (`Library::search_global` filters them out). Mirrors the GraphQL
/// `LibraryFileType` enum.
#[derive(serde::Serialize, Deserialize, Copy, Clone, Debug, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum LibraryFileType {
    Skill,
    Note,
}

impl From<LibraryFileType> for DocType {
    fn from(t: LibraryFileType) -> Self {
        match t {
            LibraryFileType::Skill => DocType::Skill,
            LibraryFileType::Note => DocType::Note,
        }
    }
}

/// Workflow rows can't reach this conversion in practice — `Library::
/// search_global` drops them before fusion. Default to Note if one ever
/// does, matching the GraphQL fallback.
impl From<DocType> for LibraryFileType {
    fn from(t: DocType) -> Self {
        match t {
            DocType::Skill => LibraryFileType::Skill,
            DocType::Note | DocType::Workflow => LibraryFileType::Note,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum LibraryParams {
    Search {
        query: String,
        #[serde(default)]
        types: Option<Vec<LibraryFileType>>,
        #[serde(default = "default_search_limit")]
        limit: usize,
    },
}

impl LibraryParams {
    fn audit_action(&self) -> &'static str {
        match self {
            Self::Search { .. } => "library.search",
        }
    }
}

#[derive(Default, serde::Serialize, schemars::JsonSchema)]
struct LibraryOutput {
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    hits: Option<Vec<LibrarySearchHit>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<usize>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct LibrarySearchHit {
    id: String,
    r#type: LibraryFileType,
    title: String,
    snippet: String,
    score: f64,
    tags: Vec<String>,
}

impl From<GlobalSearchHit> for LibrarySearchHit {
    fn from(hit: GlobalSearchHit) -> Self {
        Self {
            id: hit.doc_id.to_string(),
            r#type: hit.doc_type.into(),
            title: hit.title,
            snippet: make_snippet(&hit.content, SNIPPET_CHARS),
            score: hit.score,
            tags: hit.tags,
        }
    }
}

fn make_snippet(content: &str, max_chars: usize) -> String {
    let trimmed = content.trim();
    let mut out = String::with_capacity(max_chars + 1);
    for ch in trimmed.chars().take(max_chars) {
        out.push(ch);
    }
    if trimmed.chars().count() > max_chars {
        out.push('…');
    }
    out
}

static LIBRARY_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<LibraryOutput>);

static LIBRARY_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "enum": ["search"],
                "description": "Which library operation to perform."
            },
            "query": {
                "type": "string",
                "description": "Search query — keywords or natural language (search)."
            },
            "types": {
                "type": "array",
                "items": {
                    "type": "string",
                    "enum": ["skill", "note"]
                },
                "description": "Restrict results to these library file types. Omit / empty = all searchable types. (Workflows are git-synced but not indexed for search.)"
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "description": "Maximum number of results (default 50, capped at 200)."
            }
        },
        "required": ["command", "query"],
        "additionalProperties": false
    })
});

pub struct LibraryTool {
    library: Arc<Library>,
}

impl LibraryTool {
    pub fn new(library: Arc<Library>) -> Self {
        Self { library }
    }
}

#[async_trait::async_trait]
impl TopLevelTool for LibraryTool {
    fn name(&self) -> &str {
        "library"
    }

    fn description(&self) -> &str {
        "Cross-type, cross-workspace library search across skills and \
         notes. Always global — results span every workspace the subject \
         can read. Use this for any library lookup — alternative to \
         `use_skill` if you want to discover content without invoking it. \
         Filter `types` (skill/note, multi-select). Results are scored \
         hybrid FTS + semantic similarity. (Workflows live in the library \
         repo but are not search-indexed.)"
    }

    fn input_schema(&self) -> &serde_json::Value {
        &LIBRARY_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&LIBRARY_OUTPUT_SCHEMA)
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        !matches!(subject, AuthSubject::Anonymous)
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: LibraryParams = parse_params(arguments)?;
        Audit::record_action(params.audit_action());

        let LibraryParams::Search {
            query,
            types,
            limit,
        } = params;

        let limit = limit.clamp(1, MAX_SEARCH_LIMIT);
        let doc_types: Vec<DocType> = types
            .unwrap_or_default()
            .into_iter()
            .map(Into::into)
            .collect();

        let hits = self
            .library
            .search_global(subject, &[], &query, &doc_types, limit)
            .await?;

        let total = hits.len();
        let hits: Vec<LibrarySearchHit> = hits.into_iter().map(LibrarySearchHit::from).collect();

        let text = if hits.is_empty() {
            "No library files found matching your query.".to_string()
        } else {
            let body = hits
                .iter()
                .map(|h| {
                    format!(
                        "[{}] {} (score {:.3})\n  {}",
                        h.r#type_str(),
                        h.title,
                        h.score,
                        h.snippet,
                    )
                })
                .collect::<Vec<_>>()
                .join("\n---\n");
            format!("Found {total} library file(s):\n\n{body}")
        };

        let out = LibraryOutput {
            command: "search".to_string(),
            hits: Some(hits),
            total: Some(total),
        };
        let structured = serde_json::to_value(&out).expect("LibraryOutput serialization");
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        result.structured_content = Some(structured);
        Ok(result)
    }
}

impl LibrarySearchHit {
    fn type_str(&self) -> &'static str {
        match self.r#type {
            LibraryFileType::Skill => "skill",
            LibraryFileType::Note => "note",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_schema_has_required_fields() {
        let schema = &*LIBRARY_SCHEMA;
        let required = schema["required"].as_array().expect("required array");
        assert!(required.iter().any(|v| v == "command"));
        assert!(required.iter().any(|v| v == "query"));
    }

    #[test]
    fn parse_search_minimal() {
        let json = serde_json::json!({"command": "search", "query": "auth flow"});
        let params: LibraryParams = serde_json::from_value(json).unwrap();
        let LibraryParams::Search {
            query,
            types,
            limit,
        } = params;
        assert_eq!(query, "auth flow");
        assert!(types.is_none());
        assert_eq!(limit, DEFAULT_SEARCH_LIMIT);
    }

    #[test]
    fn schema_does_not_advertise_workspace_id() {
        let schema = &*LIBRARY_SCHEMA;
        let props = schema["properties"].as_object().expect("properties");
        assert!(
            !props.contains_key("workspace_id"),
            "library tool is global: workspace_id must not be exposed"
        );
    }

    #[test]
    fn parse_search_all_filters() {
        let json = serde_json::json!({
            "command": "search",
            "query": "x",
            "types": ["skill", "note"],
            "limit": 5
        });
        let params: LibraryParams = serde_json::from_value(json).unwrap();
        let LibraryParams::Search { types, limit, .. } = params;
        let types = types.expect("types");
        assert_eq!(types.len(), 2);
        assert!(matches!(types[0], LibraryFileType::Skill));
        assert!(matches!(types[1], LibraryFileType::Note));
        assert_eq!(limit, 5);
    }

    #[test]
    fn workflow_type_rejected_at_input_boundary() {
        let json = serde_json::json!({
            "command": "search",
            "query": "x",
            "types": ["workflow"]
        });
        assert!(serde_json::from_value::<LibraryParams>(json).is_err());
    }

    #[test]
    fn output_serializes_hits() {
        let out = LibraryOutput {
            command: "search".into(),
            hits: Some(vec![LibrarySearchHit {
                id: "11111111-1111-1111-1111-111111111111".into(),
                r#type: LibraryFileType::Skill,
                title: "auth".into(),
                snippet: "snippet".into(),
                score: 0.9,
                tags: vec!["t1".into()],
            }]),
            total: Some(1),
        };
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["command"], "search");
        assert_eq!(v["total"], 1);
        assert_eq!(v["hits"][0]["type"], "skill");
        assert_eq!(v["hits"][0]["score"], 0.9);
        assert!(
            !v["hits"][0]
                .as_object()
                .unwrap()
                .contains_key("workspace_id"),
            "tool searches are always global; workspace_id is not on hits"
        );
    }

    #[test]
    fn snippet_truncates_with_ellipsis() {
        let s = "a".repeat(300);
        let snip = make_snippet(&s, 240);
        assert_eq!(snip.chars().count(), 241);
        assert!(snip.ends_with('…'));
    }

    #[test]
    fn snippet_short_input_unchanged() {
        let snip = make_snippet("hi", 240);
        assert_eq!(snip, "hi");
    }

    #[test]
    fn searchable_doc_type_roundtrip() {
        for t in [DocType::Skill, DocType::Note] {
            let lft: LibraryFileType = t.into();
            let back: DocType = lft.into();
            assert_eq!(t.as_str(), back.as_str());
        }
    }

    #[test]
    fn workflow_doc_type_collapses_to_note() {
        let lft: LibraryFileType = DocType::Workflow.into();
        assert!(matches!(lft, LibraryFileType::Note));
    }
}
