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
use super::parse_params;

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

fn default_search_limit() -> usize {
    10
}
fn default_list_limit() -> usize {
    20
}

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum NotesParams {
    Store {
        title: String,
        content: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        note_id: Option<NoteId>,
    },
    Get {
        note_id: NoteId,
    },
    Search {
        query: String,
        #[serde(default = "default_search_limit")]
        limit: usize,
    },
    List {
        #[serde(default = "default_list_limit")]
        limit: usize,
    },
}

static NOTES_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "enum": ["store", "get", "search", "list"],
                "description": "Which notes operation to perform."
            },
            "title": {
                "type": "string",
                "description": "Title of the note (store)."
            },
            "content": {
                "type": "string",
                "description": "Note content in markdown (store)."
            },
            "tags": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Tags for categorization (store). Optional."
            },
            "note_id": {
                "type": "string",
                "format": "uuid",
                "description": "Note ID. Required for get; optional for store (omit to create, pass to update)."
            },
            "query": {
                "type": "string",
                "description": "Search query — keywords or natural language (search)."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "description": "Maximum number of results (search default 10, list default 20)."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
});

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
                    "Note {action}.\n{note}"
                ))]))
            }

            NotesParams::Get { note_id } => {
                let note = self
                    .notes
                    .find_by_id(subject, workspace_id, note_id)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;

                Ok(CallToolResult::success(vec![Content::text(note.content())]))
            }

            NotesParams::Search { query, limit } => {
                let results: Vec<SearchResult> = self
                    .notes
                    .search(subject, workspace_id, &query, limit)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;

                if results.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No notes found matching your query.",
                    )]));
                }

                let text = results
                    .iter()
                    .map(|r| r.to_string())
                    .collect::<Vec<_>>()
                    .join("---\n");
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Found {} note(s):\n\n{text}",
                    results.len(),
                ))]))
            }

            NotesParams::List { limit } => {
                let notes = self
                    .notes
                    .list(subject, workspace_id, limit)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;

                if notes.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No notes in this workspace yet.",
                    )]));
                }

                let results: Vec<SearchResult> =
                    notes.into_iter().map(SearchResult::from).collect();
                let text = results
                    .iter()
                    .map(|r| r.to_string())
                    .collect::<Vec<_>>()
                    .join("---\n");
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "{} note(s):\n\n{text}",
                    results.len(),
                ))]))
            }
        }
    }
}
