use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::library::{DocType, GlobalSearchHit, Library, LibraryFile};

use super::super::error::ToolSetsError;
use super::super::traits::{SearchableToolSet, ToolSetEntry};

const DEFAULT_SEARCH_LIMIT: usize = 50;
const MAX_SEARCH_LIMIT: usize = 200;
const MAX_GET_FILES: usize = 10;
/// Per-file body cap in the LLM-facing `text` only; `structured_content`
/// always carries the untruncated body so compose scripts have full
/// access.
const TEXT_BODY_CHARS_PER_FILE: usize = 8_000;
/// Hard ceiling on the LLM-facing `text`. Files past this are listed by
/// id-only with a `[truncated]` marker; the structured channel still
/// includes them in full.
const TEXT_TOTAL_CHARS: usize = 32_000;
const SNIPPET_CHARS: usize = 240;

fn default_search_limit() -> usize {
    DEFAULT_SEARCH_LIMIT
}

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
        obj.insert(
            "additionalProperties".into(),
            serde_json::Value::Bool(false),
        );
    }
    value
}

/// Workflows are git-synced to the library repo but excluded from search
/// (`Library::search_global` filters them out).
#[derive(serde::Serialize, Deserialize, Copy, Clone, Debug, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum LibraryFileType {
    Skill,
    Note,
    SpaceFile,
}

impl From<LibraryFileType> for DocType {
    fn from(t: LibraryFileType) -> Self {
        match t {
            LibraryFileType::Skill => DocType::Skill,
            LibraryFileType::Note => DocType::Note,
            LibraryFileType::SpaceFile => DocType::SpaceFile,
        }
    }
}

/// `Workflow` rows can't reach this conversion in practice —
/// `Library::search_global` drops them before fusion. Default to Note
/// as a defensive fallback.
impl From<DocType> for LibraryFileType {
    fn from(t: DocType) -> Self {
        match t {
            DocType::Skill => LibraryFileType::Skill,
            DocType::SpaceFile => LibraryFileType::SpaceFile,
            DocType::Note | DocType::Workflow => LibraryFileType::Note,
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SearchParams {
    /// Search query — keywords or natural language.
    query: String,
    /// Restrict results to these file types. Omit / empty = all
    /// searchable types. Workflows are git-synced but not indexed.
    #[serde(default)]
    types: Option<Vec<LibraryFileType>>,
    /// Maximum number of results (default 50, capped at 200).
    #[serde(default = "default_search_limit")]
    limit: usize,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct GetFilesParams {
    /// Library file ids to fetch — typically copied from prior `search`
    /// hits. Capped at 10 per call.
    ids: Vec<uuid::Uuid>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct SearchOutput {
    hits: Vec<LibrarySearchHit>,
    total: usize,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct GetFilesOutput {
    files: Vec<LibraryFileOutput>,
    total: usize,
    /// Ids that didn't resolve (deleted or never existed).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    missing: Vec<String>,
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

#[derive(serde::Serialize, schemars::JsonSchema)]
struct LibraryFileOutput {
    id: String,
    r#type: LibraryFileType,
    title: String,
    body: String,
    tags: Vec<String>,
    /// `null` for global content (skills with no workspace).
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    /// Populated only for `space_file` hits. The space's slug.
    #[serde(skip_serializing_if = "Option::is_none")]
    space_slug: Option<String>,
    /// Populated only for `space_file` hits. The file's path inside
    /// `spaces/<space_slug>/`.
    #[serde(skip_serializing_if = "Option::is_none")]
    relative_path: Option<String>,
}

impl From<LibraryFile> for LibraryFileOutput {
    fn from(f: LibraryFile) -> Self {
        let workspace_id = if f.workspace_id.is_nil() {
            None
        } else {
            Some(f.workspace_id.to_string())
        };
        Self {
            id: f.doc_id.to_string(),
            r#type: f.doc_type.into(),
            title: f.title,
            body: f.body,
            tags: f.tags,
            workspace_id,
            space_slug: f.space_slug,
            relative_path: f.relative_path,
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

static SEARCH_INPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<SearchParams>);
static SEARCH_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<SearchOutput>);
static GET_FILES_INPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<GetFilesParams>);
static GET_FILES_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<GetFilesOutput>);

fn tool_entry(
    name: &str,
    description: &str,
    input: serde_json::Value,
    output: serde_json::Value,
) -> ToolSetEntry {
    let input_schema: JsonObject = match input {
        serde_json::Value::Object(m) => m,
        _ => Default::default(),
    };
    let out_schema = match output {
        serde_json::Value::Object(m) => Some(Arc::new(m)),
        _ => None,
    };
    let mut tool = Tool::default();
    tool.name = name.to_string().into();
    tool.description = Some(description.to_string().into());
    tool.input_schema = Arc::new(input_schema);
    tool.output_schema = out_schema;
    ToolSetEntry {
        name: name.to_string(),
        description: tool,
        default_output_filter: None,
    }
}

pub struct LibraryToolSet {
    library: Arc<Library>,
    tools: Vec<ToolSetEntry>,
}

impl LibraryToolSet {
    pub fn new(library: Arc<Library>) -> Self {
        let tools = vec![
            tool_entry(
                "search",
                "Cross-type, cross-workspace library search across skills and notes. \
                 Hybrid FTS + semantic similarity. Always global — results span every \
                 workspace the subject can read. Returns ranked snippets; pair with \
                 `get_files` to load full bodies. Workflows are git-synced but not \
                 search-indexed.",
                (*SEARCH_INPUT_SCHEMA).clone(),
                (*SEARCH_OUTPUT_SCHEMA).clone(),
            ),
            tool_entry(
                "get_files",
                "Bulk-fetch full bodies for library file ids — typically pulled from \
                 a prior `search`. Capped at 10 ids per call. The text response \
                 truncates each body at 8k chars with a marker, but \
                 `structured_content.files[].body` is always the full untruncated \
                 text — compose scripts can read whatever they need.",
                (*GET_FILES_INPUT_SCHEMA).clone(),
                (*GET_FILES_OUTPUT_SCHEMA).clone(),
            ),
        ];
        Self { library, tools }
    }

    async fn search(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: SearchParams = parse_params(arguments)?;
        Audit::record_action("library.search");

        let limit = params.limit.clamp(1, MAX_SEARCH_LIMIT);
        let doc_types: Vec<DocType> = params
            .types
            .unwrap_or_default()
            .into_iter()
            .map(Into::into)
            .collect();

        let hits = self
            .library
            .search_global(subject, &[], &params.query, &doc_types, limit)
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
                        h.type_str(),
                        h.title,
                        h.score,
                        h.snippet,
                    )
                })
                .collect::<Vec<_>>()
                .join("\n---\n");
            format!("Found {total} library file(s):\n\n{body}")
        };

        let out = SearchOutput { hits, total };
        let structured = serde_json::to_value(&out).expect("SearchOutput serialization");
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        result.structured_content = Some(structured);
        Ok(result)
    }

    async fn get_files(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: GetFilesParams = parse_params(arguments)?;
        Audit::record_action("library.get_files");

        if params.ids.is_empty() {
            return Err(ToolSetsError::MissingArgument(
                "get_files requires a non-empty `ids` array".to_string(),
            ));
        }
        if params.ids.len() > MAX_GET_FILES {
            return Err(ToolSetsError::InvalidArgument(format!(
                "get_files capped at {MAX_GET_FILES} ids per call (got {})",
                params.ids.len()
            )));
        }

        let files = self.library.get_files(subject, &params.ids).await?;

        let returned: std::collections::HashSet<uuid::Uuid> =
            files.iter().map(|f| f.doc_id).collect();
        let missing: Vec<String> = params
            .ids
            .iter()
            .filter(|id| !returned.contains(id))
            .map(|id| id.to_string())
            .collect();

        let total = files.len();
        let files: Vec<LibraryFileOutput> =
            files.into_iter().map(LibraryFileOutput::from).collect();

        let text = render_get_files_text(&files, &missing, total);

        let out = GetFilesOutput {
            files,
            total,
            missing,
        };
        let structured = serde_json::to_value(&out).expect("GetFilesOutput serialization");
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        result.structured_content = Some(structured);
        Ok(result)
    }
}

#[async_trait::async_trait]
impl SearchableToolSet for LibraryToolSet {
    fn name(&self) -> &str {
        "library"
    }

    fn category(&self) -> &str {
        "library"
    }

    fn category_description(&self) -> &str {
        "Cross-workspace skills + notes — hybrid FTS + semantic search and bulk fetch (read-only)"
    }

    fn tools(&self) -> &[ToolSetEntry] {
        &self.tools
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        !matches!(subject, AuthSubject::Anonymous)
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        match tool_name {
            "search" => self.search(subject, arguments).await,
            "get_files" => self.get_files(subject, arguments).await,
            _ => Err(ToolSetsError::ToolNotFound(tool_name.to_string())),
        }
    }
}

impl LibrarySearchHit {
    fn type_str(&self) -> &'static str {
        type_str(self.r#type)
    }
}

impl LibraryFileOutput {
    fn type_str(&self) -> &'static str {
        type_str(self.r#type)
    }
}

fn type_str(t: LibraryFileType) -> &'static str {
    match t {
        LibraryFileType::Skill => "skill",
        LibraryFileType::Note => "note",
        LibraryFileType::SpaceFile => "space_file",
    }
}

fn render_get_files_text(files: &[LibraryFileOutput], missing: &[String], total: usize) -> String {
    if files.is_empty() {
        let supplied = total + missing.len();
        return format!("No library files found for the supplied {supplied} id(s).");
    }

    let mut out = format!("Loaded {total} library file(s):\n\n");
    let mut omitted: Vec<&str> = Vec::new();

    for (i, f) in files.iter().enumerate() {
        let header = match (&f.space_slug, &f.relative_path) {
            (Some(slug), Some(path)) => {
                format!("[{}] [{slug}] {path} — {}", f.type_str(), f.title)
            }
            _ => format!("[{}] {}", f.type_str(), f.title),
        };
        let body = truncate_with_marker(&f.body, TEXT_BODY_CHARS_PER_FILE);
        let separator = if i == 0 { "" } else { "\n---\n" };
        let chunk = format!("{separator}{header}\n\n{body}");

        if out.chars().count() + chunk.chars().count() > TEXT_TOTAL_CHARS {
            omitted.push(&f.id);
            continue;
        }
        out.push_str(&chunk);
    }

    if !omitted.is_empty() {
        out.push_str(&format!(
            "\n\n[total response cap reached — {} file(s) omitted from text but present in structured_content: {}]",
            omitted.len(),
            omitted.join(", ")
        ));
    }
    if !missing.is_empty() {
        out.push_str(&format!(
            "\n\n({} id(s) not found: {})",
            missing.len(),
            missing.join(", ")
        ));
    }
    out
}

fn truncate_with_marker(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str("\n\n[…body truncated in text view; full body in structured_content]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_search_minimal() {
        let json = serde_json::json!({"query": "auth flow"});
        let params: SearchParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.query, "auth flow");
        assert!(params.types.is_none());
        assert_eq!(params.limit, DEFAULT_SEARCH_LIMIT);
    }

    #[test]
    fn parse_get_files() {
        let json = serde_json::json!({
            "ids": [
                "11111111-1111-1111-1111-111111111111",
                "22222222-2222-2222-2222-222222222222"
            ]
        });
        let params: GetFilesParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.ids.len(), 2);
    }

    #[test]
    fn workflow_type_rejected_at_input_boundary() {
        let json = serde_json::json!({
            "query": "x",
            "types": ["workflow"]
        });
        assert!(serde_json::from_value::<SearchParams>(json).is_err());
    }

    #[test]
    fn library_file_output_omits_nil_workspace() {
        let f = LibraryFile {
            doc_id: uuid::Uuid::new_v4(),
            doc_type: DocType::Skill,
            workspace_id: uuid::Uuid::nil(),
            title: "global skill".into(),
            body: "body".into(),
            tags: vec![],
            space_slug: None,
            relative_path: None,
        };
        let out = LibraryFileOutput::from(f);
        assert!(out.workspace_id.is_none());
        let v = serde_json::to_value(&out).unwrap();
        assert!(!v.as_object().unwrap().contains_key("workspace_id"));
    }

    #[test]
    fn library_file_output_keeps_scoped_workspace() {
        let ws = uuid::Uuid::new_v4();
        let f = LibraryFile {
            doc_id: uuid::Uuid::new_v4(),
            doc_type: DocType::Note,
            workspace_id: ws,
            title: "scoped note".into(),
            body: "body".into(),
            tags: vec!["t1".into()],
            space_slug: None,
            relative_path: None,
        };
        let out = LibraryFileOutput::from(f);
        assert_eq!(out.workspace_id.as_deref(), Some(ws.to_string().as_str()));
    }

    #[test]
    fn library_file_output_carries_space_metadata() {
        let f = LibraryFile {
            doc_id: uuid::Uuid::new_v4(),
            doc_type: DocType::SpaceFile,
            workspace_id: uuid::Uuid::nil(),
            title: "Incident playbook".into(),
            body: "body".into(),
            tags: vec![],
            space_slug: Some("oncall".into()),
            relative_path: Some("runbooks/incident-foo.md".into()),
        };
        let out = LibraryFileOutput::from(f);
        assert_eq!(out.space_slug.as_deref(), Some("oncall"));
        assert_eq!(
            out.relative_path.as_deref(),
            Some("runbooks/incident-foo.md")
        );

        let header_files = vec![out];
        let rendered = render_get_files_text(&header_files, &[], 1);
        assert!(
            rendered.contains("[space_file] [oncall] runbooks/incident-foo.md — Incident playbook")
        );
    }

    #[test]
    fn text_render_truncates_oversized_body() {
        let big_body = "x".repeat(TEXT_BODY_CHARS_PER_FILE + 5_000);
        let f = LibraryFileOutput {
            id: "id1".into(),
            r#type: LibraryFileType::Skill,
            title: "huge".into(),
            body: big_body.clone(),
            tags: vec![],
            workspace_id: None,
            space_slug: None,
            relative_path: None,
        };
        let text = render_get_files_text(&[f], &[], 1);
        assert!(
            text.chars().count() < big_body.len(),
            "text view should truncate oversized body"
        );
        assert!(text.contains("body truncated in text view"));
    }

    #[test]
    fn text_render_caps_total_and_lists_omitted_ids() {
        let body = "x".repeat(12_000);
        let files: Vec<LibraryFileOutput> = (0..4)
            .map(|i| LibraryFileOutput {
                id: format!("id-{i}"),
                r#type: LibraryFileType::Note,
                title: format!("file {i}"),
                body: body.clone(),
                tags: vec![],
                workspace_id: None,
                space_slug: None,
                relative_path: None,
            })
            .collect();
        let text = render_get_files_text(&files, &[], 4);
        assert!(text.contains("total response cap reached"));
        assert!(text.contains("omitted from text but present in structured_content"));
    }

    #[test]
    fn structured_output_keeps_full_body_when_text_truncates() {
        let big_body = "y".repeat(TEXT_BODY_CHARS_PER_FILE + 1_000);
        let files = vec![LibraryFileOutput {
            id: "id1".into(),
            r#type: LibraryFileType::Skill,
            title: "huge".into(),
            body: big_body.clone(),
            tags: vec![],
            workspace_id: None,
            space_slug: None,
            relative_path: None,
        }];
        let _text = render_get_files_text(&files, &[], 1);
        let v = serde_json::to_value(&files).unwrap();
        assert_eq!(
            v[0]["body"].as_str().unwrap().chars().count(),
            big_body.len()
        );
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
