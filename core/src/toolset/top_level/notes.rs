use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::library::SearchResult;
use crate::note::Notes;
use crate::primitives::NoteId;
use crate::project::Projects;

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
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
});

async fn resolve_project_name(
    projects: &Projects,
    subject: &AuthSubject,
) -> Result<String, ToolSetsError> {
    let project_id = subject.project_id().ok_or(ToolSetsError::Unauthorized)?;
    let project = projects.find_by_id(subject, project_id).await?;
    Ok(project.name)
}

pub struct NotesTool {
    notes: Arc<Notes>,
    projects: Arc<Projects>,
}

impl NotesTool {
    pub fn new(notes: Arc<Notes>, projects: Arc<Projects>) -> Self {
        Self { notes, projects }
    }
}

#[async_trait::async_trait]
impl TopLevelTool for NotesTool {
    fn name(&self) -> &str {
        "notes"
    }

    fn description(&self) -> &str {
        "Project knowledge base — persistent memory shared across agents. \
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
        subject.project_id().is_some_and(|project| {
            subject
                .can(AuthVerb::Read, AuthResource::Note(project, None))
                .is_ok()
        })
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let project_id = subject.project_id().ok_or(ToolSetsError::Unauthorized)?;
        let params: NotesParams = parse_params(arguments)?;

        Audit::record_action(format!("notes.{}", params.command_name()));

        let (text, out) = match params {
            NotesParams::Store {
                title,
                content,
                tags,
                note_id,
            } => {
                let project_name = resolve_project_name(&self.projects, subject).await?;
                let note = self
                    .notes
                    .store_or_update(
                        subject,
                        project_id,
                        &project_name,
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
                    .find_by_id(subject, project_id, note_id)
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
                    .search(subject, project_id, &query, limit)
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
                    .pin(subject, project_id, note_id)
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
                    .unpin(subject, project_id, note_id)
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

            NotesParams::List { limit } => {
                let notes = self
                    .notes
                    .list(subject, project_id, limit)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;

                let results: Vec<SearchResult> =
                    notes.into_iter().map(SearchResult::from).collect();

                let text = if results.is_empty() {
                    "No notes in this project yet.".to_string()
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
