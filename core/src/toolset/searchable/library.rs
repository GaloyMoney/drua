use std::sync::{Arc, LazyLock};

use drua_library::{DocType, SearchHit, SearchableFields, SPACE_DOC_TYPE};
use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::library::AuthedSearch;
use crate::note::NOTE_DOC_TYPE;
use crate::skill::SKILL_DOC_TYPE;

use super::super::error::ToolSetsError;
use super::super::traits::{SearchableToolSet, ToolSetEntry};

/// Tool-shaped global search hit. Translated from `drua_library::SearchHit`
/// at the toolset boundary.
#[derive(Debug, Clone)]
struct GlobalSearchHit {
    doc_id: uuid::Uuid,
    doc_type: DocType,
    title: String,
    content: String,
    score: f64,
    /// Set only when the hit is a `space_file` row — mirrors
    /// `fields_to_file`'s slug attribution rule.
    space_slug: Option<String>,
    /// Set only when the hit is a `space_file` row.
    relative_path: Option<String>,
}

/// Tool-shaped fetched library file. Translated from
/// `drua_library::SearchableFields` at the toolset boundary.
#[derive(Debug, Clone)]
struct LibraryFile {
    doc_id: uuid::Uuid,
    doc_type: DocType,
    title: String,
    body: String,
    /// `None` for unscoped/global content (e.g. global skills).
    project_id: Option<uuid::Uuid>,
    space_slug: Option<String>,
    relative_path: Option<String>,
}

fn hit_to_global(hit: SearchHit) -> GlobalSearchHit {
    let is_space = hit.fields.doc_type == SPACE_DOC_TYPE;
    let (space_slug, relative_path) = if is_space {
        (hit.fields.scope_slug, hit.fields.path)
    } else {
        (None, None)
    };
    GlobalSearchHit {
        doc_id: hit.fields.doc_id,
        doc_type: hit.fields.doc_type,
        title: hit.fields.name,
        content: hit.fields.content,
        score: hit.score,
        space_slug,
        relative_path,
    }
}

fn fields_to_file(fields: SearchableFields) -> LibraryFile {
    let is_space = fields.doc_type == SPACE_DOC_TYPE;
    let (space_slug, relative_path, project_id) = if is_space {
        (fields.scope_slug, fields.path, None)
    } else {
        (None, None, fields.scope_id)
    };
    LibraryFile {
        doc_id: fields.doc_id,
        doc_type: fields.doc_type,
        title: fields.name,
        body: fields.content,
        project_id,
        space_slug,
        relative_path,
    }
}

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
            LibraryFileType::Skill => SKILL_DOC_TYPE,
            LibraryFileType::Note => NOTE_DOC_TYPE,
            LibraryFileType::SpaceFile => SPACE_DOC_TYPE,
        }
    }
}

/// Workflow rows are filtered out of the search before reaching the
/// toolset; non-recognised doc types default to Note as a defensive
/// fallback.
impl From<DocType> for LibraryFileType {
    fn from(t: DocType) -> Self {
        if t == SKILL_DOC_TYPE {
            LibraryFileType::Skill
        } else if t == SPACE_DOC_TYPE {
            LibraryFileType::SpaceFile
        } else {
            LibraryFileType::Note
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
    /// Populated only for `space_file` hits. The space's slug.
    #[serde(skip_serializing_if = "Option::is_none")]
    space_slug: Option<String>,
    /// Populated only for `space_file` hits. The file's path inside
    /// `spaces/<space_slug>/`.
    #[serde(skip_serializing_if = "Option::is_none")]
    relative_path: Option<String>,
}

impl From<GlobalSearchHit> for LibrarySearchHit {
    fn from(hit: GlobalSearchHit) -> Self {
        Self {
            id: hit.doc_id.to_string(),
            r#type: hit.doc_type.into(),
            title: hit.title,
            snippet: make_snippet(&hit.content, SNIPPET_CHARS),
            score: hit.score,
            tags: Vec::new(),
            space_slug: hit.space_slug,
            relative_path: hit.relative_path,
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
    /// `null` for global content (skills with no project).
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
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
        Self {
            id: f.doc_id.to_string(),
            r#type: f.doc_type.into(),
            title: f.title,
            body: f.body,
            tags: Vec::new(),
            project_id: f.project_id.map(|id| id.to_string()),
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
    search: Arc<AuthedSearch>,
    tools: Vec<ToolSetEntry>,
}

impl LibraryToolSet {
    pub fn new(search: Arc<AuthedSearch>) -> Self {
        let tools = vec![
            tool_entry(
                "search",
                "Cross-type, cross-project library search across skills, notes, \
                 and space files. Hybrid FTS + semantic similarity. Always \
                 global — results span every project the subject can read. \
                 Returns ranked snippets; `space_file` hits also carry \
                 `space_slug` and `relative_path` so callers can route to \
                 the file without a second lookup. Pair with `get_files` to \
                 load full bodies. Workflows are git-synced but not \
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
        Self { search, tools }
    }

    async fn search(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: SearchParams = parse_params(arguments)?;
        Audit::record_action("library.search");

        let limit = params.limit.clamp(1, MAX_SEARCH_LIMIT);
        // Workflows are git-synced but not search-indexed; if the
        // caller passed an empty filter we expand it to the indexed
        // doc types only so future workflow rows would still get
        // dropped here defensively.
        let doc_types: Vec<DocType> = if let Some(types) = params.types {
            types.into_iter().map(Into::into).collect()
        } else {
            vec![SKILL_DOC_TYPE, NOTE_DOC_TYPE, SPACE_DOC_TYPE]
        };

        let raw = self
            .search
            .search(subject, &[], &params.query, &doc_types, &[], limit)
            .await?;

        let hits: Vec<LibrarySearchHit> = raw
            .into_iter()
            .map(hit_to_global)
            .map(LibrarySearchHit::from)
            .collect();
        let total = hits.len();

        let text = if hits.is_empty() {
            "No library files found matching your query.".to_string()
        } else {
            let body = hits
                .iter()
                .map(|h| {
                    let header = match (&h.space_slug, &h.relative_path) {
                        (Some(slug), Some(path)) => format!(
                            "[{}] [{slug}] {path} — {} (score {:.3})",
                            h.type_str(),
                            h.title,
                            h.score,
                        ),
                        _ => format!("[{}] {} (score {:.3})", h.type_str(), h.title, h.score,),
                    };
                    format!("{header}\n  {}", h.snippet)
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

        let raw = self.search.find_by_ids(subject, &params.ids).await?;
        let files: Vec<LibraryFile> = raw.into_iter().map(fields_to_file).collect();

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
        "Cross-project skills + notes — hybrid FTS + semantic search and bulk fetch (read-only)"
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
    fn library_file_output_omits_global_project() {
        let f = LibraryFile {
            doc_id: uuid::Uuid::new_v4(),
            doc_type: SKILL_DOC_TYPE,
            project_id: None,
            title: "global skill".into(),
            body: "body".into(),
            space_slug: None,
            relative_path: None,
        };
        let out = LibraryFileOutput::from(f);
        assert!(out.project_id.is_none());
        let v = serde_json::to_value(&out).unwrap();
        assert!(!v.as_object().unwrap().contains_key("project_id"));
    }

    #[test]
    fn library_file_output_keeps_scoped_project() {
        let project = uuid::Uuid::new_v4();
        let f = LibraryFile {
            doc_id: uuid::Uuid::new_v4(),
            doc_type: NOTE_DOC_TYPE,
            project_id: Some(project),
            title: "scoped note".into(),
            body: "body".into(),
            space_slug: None,
            relative_path: None,
        };
        let out = LibraryFileOutput::from(f);
        assert_eq!(
            out.project_id.as_deref(),
            Some(project.to_string().as_str())
        );
    }

    #[test]
    fn library_file_output_carries_space_metadata() {
        let f = LibraryFile {
            doc_id: uuid::Uuid::new_v4(),
            doc_type: SPACE_DOC_TYPE,
            project_id: None,
            title: "Incident playbook".into(),
            body: "body".into(),
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
            project_id: None,
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
                project_id: None,
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
            project_id: None,
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
        for t in [SKILL_DOC_TYPE, NOTE_DOC_TYPE] {
            let lft: LibraryFileType = t.clone().into();
            let back: DocType = lft.into();
            assert_eq!(t.as_str(), back.as_str());
        }
    }

    #[test]
    fn workflow_doc_type_collapses_to_note() {
        let lft: LibraryFileType = DocType::new("workflow").into();
        assert!(matches!(lft, LibraryFileType::Note));
    }

    fn make_hit(doc_type: DocType, scope_slug: Option<&str>, path: Option<&str>) -> SearchHit {
        SearchHit {
            score: 0.5,
            fields: SearchableFields {
                doc_id: uuid::Uuid::new_v4(),
                doc_type,
                scope_id: scope_slug.map(|_| uuid::Uuid::new_v4()),
                scope_slug: scope_slug.map(str::to_string),
                name: "title".into(),
                path: path.map(str::to_string),
                content: "body".into(),
            },
        }
    }

    #[test]
    fn space_hit_carries_slug_and_path() {
        let hit = make_hit(
            SPACE_DOC_TYPE,
            Some("drua-dev"),
            Some("research/ha-readiness.md"),
        );
        let global = hit_to_global(hit);
        assert_eq!(global.space_slug.as_deref(), Some("drua-dev"));
        assert_eq!(
            global.relative_path.as_deref(),
            Some("research/ha-readiness.md")
        );
        let out = LibrarySearchHit::from(global);
        assert_eq!(out.space_slug.as_deref(), Some("drua-dev"));
        assert_eq!(
            out.relative_path.as_deref(),
            Some("research/ha-readiness.md")
        );
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["space_slug"].as_str(), Some("drua-dev"));
        assert_eq!(
            v["relative_path"].as_str(),
            Some("research/ha-readiness.md")
        );
    }

    #[test]
    fn skill_hit_omits_slug_and_path() {
        let hit = make_hit(SKILL_DOC_TYPE, Some("project-name"), Some("skill.md"));
        let global = hit_to_global(hit);
        assert!(global.space_slug.is_none());
        assert!(global.relative_path.is_none());
        let out = LibrarySearchHit::from(global);
        let v = serde_json::to_value(&out).unwrap();
        assert!(!v.as_object().unwrap().contains_key("space_slug"));
        assert!(!v.as_object().unwrap().contains_key("relative_path"));
    }

    #[test]
    fn note_hit_omits_slug_and_path() {
        let hit = make_hit(NOTE_DOC_TYPE, Some("project-name"), Some("note.md"));
        let global = hit_to_global(hit);
        assert!(global.space_slug.is_none());
        assert!(global.relative_path.is_none());
    }

    #[test]
    fn search_output_schema_advertises_slug_and_path() {
        let schema = &*SEARCH_OUTPUT_SCHEMA;
        let s = schema.to_string();
        assert!(s.contains("space_slug"));
        assert!(s.contains("relative_path"));
    }
}
