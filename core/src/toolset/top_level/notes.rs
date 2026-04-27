use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::library::SearchResult;
use crate::note::Notes;
use crate::primitives::{NoteId, WorkflowDefinitionId};
use crate::workspace::Workspaces;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

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
        /// When `Some`, the note is scoped to that workflow definition
        /// (runbook-style context for that workflow). Honoured on
        /// create only — `update` cannot move a note between scopes.
        #[serde(default)]
        workflow_id: Option<WorkflowDefinitionId>,
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
        /// `Some` filters the listing to a specific workflow
        /// definition; omit (or pass `null`) for the default
        /// workspace-only listing.
        #[serde(default)]
        workflow_id: Option<WorkflowDefinitionId>,
    },
    Pin {
        note_id: NoteId,
    },
    Unpin {
        note_id: NoteId,
    },
}

impl NotesParams {
    fn command_name(&self) -> &'static str {
        match self {
            Self::Store { note_id, .. } => {
                if note_id.is_some() {
                    "update"
                } else {
                    "create"
                }
            }
            Self::Get { .. } => "get",
            Self::Search { .. } => "search",
            Self::List { .. } => "list",
            Self::Pin { .. } => "pin",
            Self::Unpin { .. } => "unpin",
        }
    }
}

/// Union output for all notes subcommands; fields are populated per subcommand.
#[derive(Default, serde::Serialize, schemars::JsonSchema)]
struct NotesOutput {
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    note_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    results: Option<Vec<NoteResultOutput>>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct NoteResultOutput {
    note_id: String,
    title: String,
    preview: String,
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<f64>,
}

static NOTES_OUTPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<NotesOutput>);

static NOTES_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "enum": ["store", "get", "search", "list", "pin", "unpin"],
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
                "description": "Note ID. Required for get/pin/unpin; optional for store (omit to create, pass to update)."
            },
            "query": {
                "type": "string",
                "description": "Search query — keywords or natural language (search)."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "description": "Maximum number of results (search default 10, list default 20)."
            },
            "workflow_id": {
                "type": "string",
                "format": "uuid",
                "description": "Workflow definition ID. On `store` (omit `note_id`): scopes the new note to that workflow as runbook context. On `list`: filters to notes attached to that workflow only."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
});

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
        "Workspace knowledge base — persistent memory shared across agents. \
         Store findings, decisions, and task outcomes so future agents benefit. \
         Commands: `store` (create/update), `get` (by ID), \
         `search` (hybrid keyword + semantic), `list` (recent first), \
         `pin` (inject into all agents' context), `unpin` (remove from context)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &NOTES_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&NOTES_OUTPUT_SCHEMA)
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

        Audit::record_action(format!("notes.{}", params.command_name()));

        let (text, out) = match params {
            NotesParams::Store {
                title,
                content,
                tags,
                note_id,
                workflow_id,
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
                        workflow_id,
                    )
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;

                let action = if note_id.is_some() {
                    "updated"
                } else {
                    "created"
                };
                let text = format!("Note {action}.\n{note}");
                let out = NotesOutput {
                    command: action.to_string(),
                    note_id: Some(note.id.to_string()),
                    title: Some(note.title.clone()),
                    tags: Some(note.tags.clone()),
                    ..Default::default()
                };
                (text, out)
            }

            NotesParams::Get { note_id } => {
                let note = self
                    .notes
                    .find_by_id(subject, workspace_id, note_id)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;

                let text = note.content().to_string();
                let out = NotesOutput {
                    command: "get".to_string(),
                    note_id: Some(note.id.to_string()),
                    title: Some(note.title.clone()),
                    content: Some(note.content.clone()),
                    tags: Some(note.tags.clone()),
                    ..Default::default()
                };
                (text, out)
            }

            NotesParams::Search { query, limit } => {
                let results: Vec<SearchResult> = self
                    .notes
                    .search(subject, workspace_id, &query, limit)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;

                let text = if results.is_empty() {
                    "No notes found matching your query.".to_string()
                } else {
                    let body = results
                        .iter()
                        .map(|r| r.to_string())
                        .collect::<Vec<_>>()
                        .join("---\n");
                    format!("Found {} note(s):\n\n{body}", results.len())
                };
                let out = NotesOutput {
                    command: "search".to_string(),
                    results: Some(
                        results
                            .iter()
                            .map(|r| NoteResultOutput {
                                note_id: r.doc_id.to_string(),
                                title: r.title.clone(),
                                preview: r.content.chars().take(200).collect(),
                                tags: r.tags.clone(),
                                score: Some(r.score),
                            })
                            .collect(),
                    ),
                    ..Default::default()
                };
                (text, out)
            }

            NotesParams::Pin { note_id } => {
                let note = self
                    .notes
                    .pin(subject, workspace_id, note_id)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;
                let text = format!("Note pinned.\n{note}");
                let out = NotesOutput {
                    command: "pin".to_string(),
                    note_id: Some(note.id.to_string()),
                    title: Some(note.title.clone()),
                    pinned: Some(true),
                    ..Default::default()
                };
                (text, out)
            }

            NotesParams::Unpin { note_id } => {
                let note = self
                    .notes
                    .unpin(subject, workspace_id, note_id)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;
                let text = format!("Note unpinned.\n{note}");
                let out = NotesOutput {
                    command: "unpin".to_string(),
                    note_id: Some(note.id.to_string()),
                    title: Some(note.title.clone()),
                    pinned: Some(false),
                    ..Default::default()
                };
                (text, out)
            }

            NotesParams::List { limit, workflow_id } => {
                let notes = match workflow_id {
                    Some(wf) => self
                        .notes
                        .list_for_workflow_definition(subject, workspace_id, wf)
                        .await
                        .map_err(|e| ToolSetsError::Note(e.to_string()))?,
                    None => self
                        .notes
                        .list(subject, workspace_id, limit)
                        .await
                        .map_err(|e| ToolSetsError::Note(e.to_string()))?,
                };

                let results: Vec<SearchResult> =
                    notes.into_iter().map(SearchResult::from).collect();

                let text = if results.is_empty() {
                    match workflow_id {
                        Some(wf) => format!("No notes attached to workflow {wf}."),
                        None => "No notes in this workspace yet.".to_string(),
                    }
                } else {
                    let body = results
                        .iter()
                        .map(|r| r.to_string())
                        .collect::<Vec<_>>()
                        .join("---\n");
                    format!("{} note(s):\n\n{body}", results.len())
                };
                let out = NotesOutput {
                    command: "list".to_string(),
                    results: Some(
                        results
                            .iter()
                            .map(|r| NoteResultOutput {
                                note_id: r.doc_id.to_string(),
                                title: r.title.clone(),
                                preview: r.content.chars().take(200).collect(),
                                tags: r.tags.clone(),
                                score: None,
                            })
                            .collect(),
                    ),
                    ..Default::default()
                };
                (text, out)
            }
        };

        let structured = serde_json::to_value(&out).expect("NotesOutput serialization");
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        result.structured_content = Some(structured);
        Ok(result)
    }
}
