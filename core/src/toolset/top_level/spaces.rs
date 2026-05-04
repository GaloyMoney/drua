use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use drua_library::Space;

use crate::audit::Audit;
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::library::AuthedSpaces;
use crate::project::Projects;
use crate::space_fs::SpaceFs;

use super::super::error::ToolSetsError;
use super::super::inspect::{dispatch_inspect, require_space_op, InspectTool};
use super::super::traits::TopLevelTool;
use super::{parse_params, OutputSchema};

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum SpacesParams {
    Create {
        slug: String,
        #[serde(default)]
        description: Option<String>,
    },
    /// Mounts an existing space onto the calling agent's project so
    /// it shows up in `<spaces>` and is accessible via `space:<slug>/`
    /// paths.
    Mount { slug: String },
    /// Drops a space from the calling agent's project. Idempotent;
    /// the space itself is unaffected.
    Unmount { slug: String },
    /// Lists spaces. Defaults to spaces mounted on the caller's
    /// project; `all: true` returns every space in the library
    /// (used to discover candidates before `mount`).
    List {
        #[serde(default)]
        all: bool,
    },
    /// Read-only file ops on a space mounted on the caller's project.
    /// Mirrors `sandbox`'s `inspect`: pass `tool` (read|ls|grep|glob)
    /// and `tool_args`.
    Inspect {
        slug: String,
        tool: InspectTool,
        #[serde(default)]
        tool_args: Option<JsonObject>,
    },
    /// Blind overwrite of `space:<slug>/<path>` with `content`. Slug
    /// must be mounted on the caller's project.
    Write {
        slug: String,
        path: String,
        content: String,
    },
    /// Delete `space:<slug>/<path>`. Slug must be mounted on the
    /// caller's project.
    Delete { slug: String, path: String },
    /// Rename / move `space:<slug>/<from>` → `space:<slug>/<to>`.
    /// Slug must be mounted on the caller's project.
    Move {
        slug: String,
        from: String,
        to: String,
    },
}

impl SpacesParams {
    fn command_name(&self) -> &'static str {
        match self {
            Self::Create { .. } => "create",
            Self::Mount { .. } => "mount",
            Self::Unmount { .. } => "unmount",
            Self::List { .. } => "list",
            Self::Inspect { .. } => "inspect",
            Self::Write { .. } => "write",
            Self::Delete { .. } => "delete",
            Self::Move { .. } => "move",
        }
    }
}

#[derive(Default, serde::Serialize, schemars::JsonSchema)]
struct SpaceSummary {
    id: String,
    slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

impl From<&Space> for SpaceSummary {
    fn from(s: &Space) -> Self {
        Self {
            id: s.id.to_string(),
            slug: s.slug.clone(),
            description: s.description.clone(),
        }
    }
}

#[derive(Default, serde::Serialize, schemars::JsonSchema)]
struct SpacesOutput {
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    space: Option<SpaceSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    spaces: Option<Vec<SpaceSummary>>,
}

static SPACES_OUTPUT: LazyLock<OutputSchema<SpacesOutput>> = LazyLock::new(OutputSchema::new);

static SPACES_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "enum": ["create", "mount", "unmount", "list", "inspect", "write", "delete", "move"],
                "description": "Which spaces operation to perform."
            },
            "slug": {
                "type": "string",
                "description": "Directory-safe identifier ([a-z0-9-]+, no leading/trailing hyphens). Becomes spaces/<slug>/ in the library repo. Required for create, mount, unmount, inspect, write, delete, and move."
            },
            "description": {
                "type": "string",
                "description": "Human-readable summary of the space's purpose. Used by create only."
            },
            "all": {
                "type": "boolean",
                "description": "List flag: true returns every space in the library (for discovery before mount); false (default) returns only spaces mounted on this project."
            },
            "tool": {
                "type": "string",
                "enum": ["read", "ls", "grep", "glob"],
                "description": "Inspect sub-tool. Required for inspect."
            },
            "tool_args": {
                "type": "object",
                "description": "Inspect sub-tool arguments. Shape mirrors the equivalent top-level tool: ls/read take {path, ...}; grep/glob take {pattern, path?, ...}."
            },
            "path": {
                "type": "string",
                "description": "Path relative to spaces/<slug>/. Required for write and delete."
            },
            "content": {
                "type": "string",
                "description": "File contents. Required for write."
            },
            "from": {
                "type": "string",
                "description": "Source path relative to spaces/<slug>/. Required for move."
            },
            "to": {
                "type": "string",
                "description": "Destination path relative to spaces/<slug>/. Required for move."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
});

pub struct SpacesTool {
    spaces: Arc<AuthedSpaces>,
    projects: Arc<Projects>,
    space_fs: Arc<SpaceFs>,
}

impl SpacesTool {
    pub fn new(spaces: Arc<AuthedSpaces>, projects: Arc<Projects>, space_fs: Arc<SpaceFs>) -> Self {
        Self {
            spaces,
            projects,
            space_fs,
        }
    }
}

#[async_trait::async_trait]
impl TopLevelTool for SpacesTool {
    fn name(&self) -> &str {
        "spaces"
    }

    fn description(&self) -> &str {
        "Manage library spaces — bounded collaborative folders under \
         `spaces/<slug>/` in the knowledge-base repo. Commands: \
         `create` (requires `slug`, optional `description`; auto-mounts \
         the new space onto the caller's project), \
         `mount` (requires `slug`; declares that the caller's project \
         can see the space), \
         `unmount` (requires `slug`; drops a previously-mounted space \
         — the space itself is unaffected), \
         `list` (defaults to spaces mounted by the caller's project; \
         pass `all: true` to discover every space in the library), \
         `inspect` (read-only file ops on a mounted space; requires \
         `slug`, `tool` (read|ls|grep|glob), `tool_args`), \
         `write` (requires `slug`, `path`, `content`), \
         `delete` (requires `slug`, `path`), \
         `move` (requires `slug`, `from`, `to`). \
         File ops are gated on the slug being mounted on the caller's \
         project."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &SPACES_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(SPACES_OUTPUT.schema())
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        // Project leads only — the tool mutates the project's
        // `mounted_spaces` set (via create / mount / unmount), and even
        // the `list` command is conceptually about administering what
        // the project sees. `Update on Project(P)` matches `ProjectAdmin`
        // (the lead-agent scope) without granting access to ordinary
        // task agents (`ProjectMember`).
        subject.project_id().is_some_and(|p| {
            subject
                .can(AuthVerb::Update, AuthResource::Project(Some(p)))
                .is_ok()
        })
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let project_id = subject.project_id().ok_or(ToolSetsError::Unauthorized)?;
        let params: SpacesParams = parse_params(arguments)?;
        Audit::record_action(format!("spaces.{}", params.command_name()));

        let (text, out) = match params {
            SpacesParams::Inspect {
                slug,
                tool,
                tool_args,
            } => {
                return dispatch_inspect(
                    &self.space_fs,
                    subject,
                    &slug,
                    tool,
                    tool_args.unwrap_or_default(),
                )
                .await;
            }
            SpacesParams::Write {
                slug,
                path,
                content,
            } => {
                let space_path = format!("space:{slug}/{path}");
                let result = self
                    .space_fs
                    .write_file(subject, &space_path, content)
                    .await?;
                require_space_op(result, "write")?;
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Wrote {space_path}"
                ))]));
            }
            SpacesParams::Delete { slug, path } => {
                let space_path = format!("space:{slug}/{path}");
                let result = self.space_fs.delete_file(subject, &space_path).await?;
                require_space_op(result, "delete")?;
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Deleted {space_path}"
                ))]));
            }
            SpacesParams::Move { slug, from, to } => {
                let from_path = format!("space:{slug}/{from}");
                let to_path = format!("space:{slug}/{to}");
                let result = self
                    .space_fs
                    .move_file(subject, &from_path, &to_path)
                    .await?;
                require_space_op(result, "move")?;
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Moved {from_path} -> {to_path}"
                ))]));
            }
            SpacesParams::Create { slug, description } => {
                let space = self
                    .projects
                    .create_and_mount_space(subject, project_id, slug, description)
                    .await?;

                let text = format!("Space created.\n  id: {}\n  slug: {}", space.id, space.slug);
                let out = SpacesOutput {
                    command: "create".to_string(),
                    space: Some(SpaceSummary::from(&space)),
                    spaces: None,
                };
                (text, out)
            }
            SpacesParams::Mount { slug } => {
                let space = self
                    .projects
                    .mount_space(subject, project_id, &slug)
                    .await?;

                let text = format!(
                    "Space mounted onto project {}.\n  slug: {}",
                    project_id, space.slug
                );
                let out = SpacesOutput {
                    command: "mount".to_string(),
                    space: Some(SpaceSummary::from(&space)),
                    spaces: None,
                };
                (text, out)
            }
            SpacesParams::Unmount { slug } => {
                let space = self.spaces.find_by_slug(&slug).await?.ok_or_else(|| {
                    ToolSetsError::Library(
                        drua_library::SpaceError::NotFound { slug: slug.clone() }.into(),
                    )
                })?;
                self.projects
                    .unmount_space(subject, project_id, space.id)
                    .await?;

                let text = format!(
                    "Space unmounted from project {}.\n  slug: {}",
                    project_id, space.slug
                );
                let out = SpacesOutput {
                    command: "unmount".to_string(),
                    space: Some(SpaceSummary::from(&space)),
                    spaces: None,
                };
                (text, out)
            }
            SpacesParams::List { all } => {
                let spaces = if all {
                    self.spaces.list_all(subject).await?
                } else {
                    self.projects
                        .list_mounted_spaces(subject, project_id)
                        .await?
                };

                let summaries: Vec<SpaceSummary> = spaces.iter().map(SpaceSummary::from).collect();
                let header = if all {
                    format!("All library spaces ({}):", summaries.len())
                } else {
                    format!("Mounted spaces ({}):", summaries.len())
                };
                let text = if summaries.is_empty() {
                    if all {
                        "No spaces in the library.".to_string()
                    } else {
                        "No spaces mounted on this project.".to_string()
                    }
                } else {
                    let lines: Vec<String> = summaries
                        .iter()
                        .map(|s| match &s.description {
                            Some(d) => format!("  - {} — {}", s.slug, d),
                            None => format!("  - {}", s.slug),
                        })
                        .collect();
                    format!("{header}\n{}", lines.join("\n"))
                };
                let out = SpacesOutput {
                    command: "list".to_string(),
                    space: None,
                    spaces: Some(summaries),
                };
                (text, out)
            }
        };

        Ok(SPACES_OUTPUT.success(text, &out))
    }
}
