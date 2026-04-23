use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::auth::AuthSubject;
use crate::library::SearchResult;
use crate::note::Notes;
use crate::primitives::NoteId;
use crate::workspace::Workspaces;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

fn default_search_limit() -> usize {
    10
}
fn default_list_limit() -> usize {
    20
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(tag = "command", rename_all = "snake_case")]
enum NotesParams {
    /// Create or update a note.
    Store {
        /// Title of the note.
        title: String,
        /// Note content (markdown).
        content: String,
        /// Tags for categorization.
        #[serde(default)]
        tags: Vec<String>,
        /// Pass to update an existing note; omit to create a new one.
        #[schemars(with = "Option<uuid::Uuid>")]
        #[serde(default)]
        note_id: Option<NoteId>,
    },
    /// Retrieve a single note by ID.
    Get {
        /// ID of the note to retrieve.
        #[schemars(with = "uuid::Uuid")]
        note_id: NoteId,
    },
    /// Hybrid keyword + semantic search.
    Search {
        /// Search query (keywords or natural language).
        query: String,
        /// Maximum number of results.
        #[serde(default = "default_search_limit")]
        limit: usize,
    },
    /// List all notes, most recent first.
    List {
        /// Maximum number of notes to return.
        #[serde(default = "default_list_limit")]
        limit: usize,
    },
}

static NOTES_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<NotesParams>);

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
    format!("id: {}\ntitle: {}{}", note.id, note.title, tags)
}

fn format_search_result(r: &SearchResult) -> String {
    let tags = if r.tags.is_empty() {
        String::new()
    } else {
        format!("  tags: {}\n", r.tags.join(", "))
    };
    let content_preview: String = r.content.chars().take(200).collect();
    format!(
        "id: {}\ntitle: {}\n{}preview: {}\n",
        r.doc_id, r.title, tags, content_preview
    )
}

fn format_result_list(results: &[SearchResult]) -> String {
    results
        .iter()
        .map(format_search_result)
        .collect::<Vec<_>>()
        .join("---\n")
}

// ---------------------------------------------------------------------------
// NotesTool
// ---------------------------------------------------------------------------

pub struct NotesTool {
    notes: Arc<Notes>,
    workspaces: Arc<Workspaces>,
}

impl NotesTool {
    pub fn new(notes: Arc<Notes>, workspaces: Arc<Workspaces>) -> Self {
        Self { notes, workspaces }
    }
}

#[async_trait::async_trait]
impl TopLevelTool for NotesTool {
    fn name(&self) -> &str {
        "notes"
    }

    fn description(&self) -> &str {
        "Workspace knowledge base. Commands: `store` (create/update a note), \
         `get` (retrieve by ID), `search` (hybrid keyword + semantic search), \
         `list` (most recent first)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &NOTES_SCHEMA
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
        let params: NotesParams = parse_params(arguments)?;

        match params {
            NotesParams::Store {
                title,
                content,
                tags,
                note_id,
            } => {
                let workspace_name = resolve_workspace_name(&self.workspaces, subject).await?;
                let note = self
                    .notes
                    .store_or_update(
                        subject,
                        workspace_id,
                        &workspace_name,
                        note_id,
                        title,
                        content,
                        tags,
                    )
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;

                let action = if note_id.is_some() {
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

            NotesParams::Get { note_id } => {
                let note = self
                    .notes
                    .find_by_id(subject, workspace_id, note_id)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;

                Ok(CallToolResult::success(vec![Content::text(format!(
                    "{}\n\n{}",
                    format_note(&note),
                    note.content
                ))]))
            }

            NotesParams::Search { query, limit } => {
                let results = self
                    .notes
                    .search(subject, workspace_id, &query, limit)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;

                if results.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No notes found matching your query.",
                    )]));
                }

                // @@ how are the results formatted? how many lines are shown?
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Found {} note(s):\n\n{}",
                    results.len(),
                    format_result_list(&results)
                ))]))
            }

            NotesParams::List { limit } => {
                let results = self
                    .notes
                    .list(subject, workspace_id, limit)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;

                if results.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No notes in this workspace yet.",
                    )]));
                }

                Ok(CallToolResult::success(vec![Content::text(format!(
                    "{} note(s):\n\n{}",
                    results.len(),
                    format_result_list(&results)
                ))]))
            }
        }
    }
}
