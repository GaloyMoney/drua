use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
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

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum NotesCommand {
    Store,
    Get,
    Search,
    List,
    Pin,
    Unpin,
    Delete,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotesInput {
    command: NotesCommand,
    #[serde(default)]
    note_id: Option<NoteId>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

fn missing(field: &str, command: &str) -> ToolSetsError {
    ToolSetsError::Note(format!(
        "notes.{command}: `{field}` is required for command={command}"
    ))
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
                "enum": ["store", "get", "search", "list", "pin", "unpin", "delete"],
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
                "description": "Tags for categorization (store)."
            },
            "note_id": {
                "type": "string",
                "format": "uuid",
                "description": "Note ID. Required for get/pin/unpin/delete; optional for store (omit to create, pass to update)."
            },
            "query": {
                "type": "string",
                "description": "Search query (search)."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "description": "Maximum results. Defaults: search=10, list=20."
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
         `pin` (inject into all agents' context), `unpin` (remove from context), \
         `delete` (requires `note_id`; project-scoped only — for space-scoped \
         notes use the `spaces` tool)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &NOTES_SCHEMA
    }

    fn inner_output_schema(&self) -> Option<&serde_json::Value> {
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
        let input: NotesInput = parse_params(arguments)?;

        let action_name = match (input.command, input.note_id.is_some()) {
            (NotesCommand::Store, true) => "update",
            (NotesCommand::Store, false) => "create",
            (NotesCommand::Get, _) => "get",
            (NotesCommand::Search, _) => "search",
            (NotesCommand::List, _) => "list",
            (NotesCommand::Pin, _) => "pin",
            (NotesCommand::Unpin, _) => "unpin",
            (NotesCommand::Delete, _) => "delete",
        };
        Audit::record_action(format!("notes.{action_name}"));

        let (text, out) = match input.command {
            NotesCommand::Store => {
                let title = input.title.ok_or_else(|| missing("title", "store"))?;
                let content = input.content.ok_or_else(|| missing("content", "store"))?;
                let tags = input.tags.unwrap_or_default();
                let note_id = input.note_id;
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

            NotesCommand::Get => {
                let note_id = input.note_id.ok_or_else(|| missing("note_id", "get"))?;
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

            NotesCommand::Search => {
                let query = input.query.ok_or_else(|| missing("query", "search"))?;
                let limit = input.limit.unwrap_or_else(default_search_limit);
                let hits = self
                    .notes
                    .search(subject, project_id, &query, limit)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;

                let text = if hits.is_empty() {
                    "No notes found matching your query.".to_string()
                } else {
                    let body = hits
                        .iter()
                        .map(|h| {
                            format!(
                                "id: {}\ntitle: {}\npreview: {}",
                                h.fields.doc_id,
                                h.fields.name,
                                h.fields.content.chars().take(200).collect::<String>(),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("---\n");
                    format!("Found {} note(s):\n\n{body}", hits.len())
                };
                let out = NotesOutput {
                    command: "search".to_string(),
                    results: Some(
                        hits.iter()
                            .map(|h| NoteResultOutput {
                                note_id: h.fields.doc_id.to_string(),
                                title: h.fields.name.clone(),
                                preview: h.fields.content.chars().take(200).collect(),
                                tags: Vec::new(),
                                score: Some(h.score),
                            })
                            .collect(),
                    ),
                    ..Default::default()
                };
                (text, out)
            }

            NotesCommand::Pin => {
                let note_id = input.note_id.ok_or_else(|| missing("note_id", "pin"))?;
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

            NotesCommand::Unpin => {
                let note_id = input.note_id.ok_or_else(|| missing("note_id", "unpin"))?;
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

            NotesCommand::Delete => {
                let note_id = input.note_id.ok_or_else(|| missing("note_id", "delete"))?;
                self.notes
                    .delete(subject, project_id, note_id)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;
                let text = format!("Note deleted (id {note_id}).");
                let out = NotesOutput {
                    command: "delete".to_string(),
                    note_id: Some(note_id.to_string()),
                    ..Default::default()
                };
                (text, out)
            }

            NotesCommand::List => {
                let limit = input.limit.unwrap_or_else(default_list_limit);
                let notes = self
                    .notes
                    .list(subject, project_id, limit)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;

                let text = if notes.is_empty() {
                    "No notes in this project yet.".to_string()
                } else {
                    let body = notes
                        .iter()
                        .map(|n| {
                            let mut s = format!("id: {}\ntitle: {}", n.id, n.title());
                            if !n.tags().is_empty() {
                                s.push_str(&format!("\n  tags: {}", n.tags().join(", ")));
                            }
                            let preview: String = n.body().chars().take(200).collect();
                            s.push_str(&format!("\npreview: {preview}"));
                            s
                        })
                        .collect::<Vec<_>>()
                        .join("---\n");
                    format!("{} note(s):\n\n{body}", notes.len())
                };
                let out = NotesOutput {
                    command: "list".to_string(),
                    results: Some(
                        notes
                            .iter()
                            .map(|n| NoteResultOutput {
                                note_id: n.id.to_string(),
                                title: n.title().to_string(),
                                preview: n.body().chars().take(200).collect(),
                                tags: n.tags().to_vec(),
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
