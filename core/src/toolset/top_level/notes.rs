use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::auth::AuthSubject;
use crate::note::{NoteSearchResult, Notes};
use crate::primitives::NoteId;
use crate::workspace::Workspaces;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct StoreNoteParams {
    /// Title of the note.
    title: String,
    /// Note content (markdown).
    content: String,
    /// Optional tags for categorization.
    #[serde(default)]
    tags: Vec<String>,
    /// If provided, update the existing note instead of creating a new one.
    #[schemars(with = "Option<uuid::Uuid>")]
    note_id: Option<NoteId>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SearchNotesParams {
    /// Search query (keywords or natural language).
    query: String,
    /// Maximum number of results (default 10).
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListNotesParams {
    /// Maximum number of notes to return (default 20).
    #[serde(default = "default_list_limit")]
    limit: usize,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct GetNoteParams {
    /// ID of the note to retrieve.
    #[schemars(with = "uuid::Uuid")]
    note_id: NoteId,
}

fn default_limit() -> usize {
    10
}
fn default_list_limit() -> usize {
    20
}

// ---------------------------------------------------------------------------
// Schemas
// ---------------------------------------------------------------------------

static STORE_NOTE_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<StoreNoteParams>);
static SEARCH_NOTES_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<SearchNotesParams>);
static LIST_NOTES_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<ListNotesParams>);
static GET_NOTE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<GetNoteParams>);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn resolve_workspace_name(
    workspaces: &Workspaces,
    subject: &AuthSubject,
) -> Result<String, ToolSetsError> {
    let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
    let ws = workspaces
        .find_by_id(subject, workspace_id)
        .await
        .map_err(|e| ToolSetsError::Workspace(e.to_string()))?;
    Ok(ws.name)
}

fn format_note(note: &crate::note::Note) -> String {
    let tags = if note.tags.is_empty() {
        String::new()
    } else {
        format!("\ntags: {}", note.tags.join(", "))
    };
    format!(
        "id: {}\ntitle: {}{}",
        note.id, note.title, tags
    )
}

fn format_search_result(r: &NoteSearchResult) -> String {
    let tags = if r.tags.is_empty() {
        String::new()
    } else {
        format!("  tags: {}\n", r.tags.join(", "))
    };
    let content_preview: String = r.content.chars().take(200).collect();
    format!(
        "id: {}\ntitle: {}\n{}preview: {}\n",
        r.id, r.title, tags, content_preview
    )
}

// ---------------------------------------------------------------------------
// store_note
// ---------------------------------------------------------------------------

pub struct StoreNote {
    notes: Arc<Notes>,
    workspaces: Arc<Workspaces>,
}

impl StoreNote {
    pub fn new(notes: Arc<Notes>, workspaces: Arc<Workspaces>) -> Self {
        Self { notes, workspaces }
    }
}

#[async_trait::async_trait]
impl TopLevelTool for StoreNote {
    fn name(&self) -> &str {
        "store_note"
    }

    fn description(&self) -> &str {
        "Create or update a workspace note. Notes are persisted, indexed for search, \
         and written to the library repo as markdown files. Pass note_id to update \
         an existing note; omit it to create a new one."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &STORE_NOTE_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.workspace_id().is_some()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
        let workspace_name = resolve_workspace_name(&self.workspaces, subject).await?;
        let params: StoreNoteParams = parse_params(arguments)?;

        let note = self
            .notes
            .store_or_update(
                subject,
                workspace_id,
                &workspace_name,
                params.note_id,
                params.title,
                params.content,
                params.tags,
            )
            .await
            .map_err(|e| ToolSetsError::Note(e.to_string()))?;

        let action = if params.note_id.is_some() {
            "updated"
        } else {
            "created"
        };
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Note {}.\n{}",
            action,
            format_note(&note)
        ))]))
    }
}

// ---------------------------------------------------------------------------
// search_notes
// ---------------------------------------------------------------------------

pub struct SearchNotes {
    notes: Arc<Notes>,
}

impl SearchNotes {
    pub fn new(notes: Arc<Notes>) -> Self {
        Self { notes }
    }
}

#[async_trait::async_trait]
impl TopLevelTool for SearchNotes {
    fn name(&self) -> &str {
        "search_notes"
    }

    fn description(&self) -> &str {
        "Search workspace notes using hybrid keyword + semantic search. \
         Returns matching notes ranked by relevance."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &SEARCH_NOTES_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.workspace_id().is_some()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
        let params: SearchNotesParams = parse_params(arguments)?;

        let results = self
            .notes
            .search(subject, workspace_id, &params.query, params.limit)
            .await
            .map_err(|e| ToolSetsError::Note(e.to_string()))?;

        if results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No notes found matching your query.",
            )]));
        }

        let text = results
            .iter()
            .map(format_search_result)
            .collect::<Vec<_>>()
            .join("---\n");

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Found {} note(s):\n\n{}",
            results.len(),
            text
        ))]))
    }
}

// ---------------------------------------------------------------------------
// list_notes
// ---------------------------------------------------------------------------

pub struct ListNotes {
    notes: Arc<Notes>,
}

impl ListNotes {
    pub fn new(notes: Arc<Notes>) -> Self {
        Self { notes }
    }
}

#[async_trait::async_trait]
impl TopLevelTool for ListNotes {
    fn name(&self) -> &str {
        "list_notes"
    }

    fn description(&self) -> &str {
        "List all notes in the workspace, most recent first."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &LIST_NOTES_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.workspace_id().is_some()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
        let params: ListNotesParams = parse_params(arguments)?;

        let results = self
            .notes
            .list(subject, workspace_id, params.limit)
            .await
            .map_err(|e| ToolSetsError::Note(e.to_string()))?;

        if results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No notes in this workspace yet.",
            )]));
        }

        let text = results
            .iter()
            .map(format_search_result)
            .collect::<Vec<_>>()
            .join("---\n");

        Ok(CallToolResult::success(vec![Content::text(format!(
            "{} note(s):\n\n{}",
            results.len(),
            text
        ))]))
    }
}

// ---------------------------------------------------------------------------
// get_note
// ---------------------------------------------------------------------------

pub struct GetNote {
    notes: Arc<Notes>,
}

impl GetNote {
    pub fn new(notes: Arc<Notes>) -> Self {
        Self { notes }
    }
}

#[async_trait::async_trait]
impl TopLevelTool for GetNote {
    fn name(&self) -> &str {
        "get_note"
    }

    fn description(&self) -> &str {
        "Retrieve a single note by its ID. Returns the full title, content, and tags."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &GET_NOTE_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.workspace_id().is_some()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
        let params: GetNoteParams = parse_params(arguments)?;

        let note = self
            .notes
            .find_by_id(subject, workspace_id, params.note_id)
            .await
            .map_err(|e| ToolSetsError::Note(e.to_string()))?;

        let tags = if note.tags.is_empty() {
            String::new()
        } else {
            format!("\ntags: {}", note.tags.join(", "))
        };

        Ok(CallToolResult::success(vec![Content::text(format!(
            "id: {}\ntitle: {}{}\n\n{}",
            note.id, note.title, tags, note.content
        ))]))
    }
}
