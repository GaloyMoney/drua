use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::space::Spaces;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum SpacesParams {
    Create {
        slug: String,
        #[serde(default)]
        description: Option<String>,
    },
}

impl SpacesParams {
    fn command_name(&self) -> &'static str {
        match self {
            Self::Create { .. } => "create",
        }
    }
}

#[derive(Default, serde::Serialize, schemars::JsonSchema)]
struct SpacesOutput {
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    space_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    authorized_workspaces: Option<Vec<String>>,
}

static SPACES_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<SpacesOutput>);

static SPACES_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "enum": ["create"],
                "description": "Which spaces operation to perform."
            },
            "slug": {
                "type": "string",
                "description": "Directory-safe identifier ([a-z0-9-]+, no leading/trailing hyphens). Becomes spaces/<slug>/ in the library repo."
            },
            "description": {
                "type": "string",
                "description": "Human-readable summary of the space's purpose. Optional."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
});

pub struct SpacesTool {
    spaces: Arc<Spaces>,
}

impl SpacesTool {
    pub fn new(spaces: Arc<Spaces>) -> Self {
        Self { spaces }
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
         `create` (requires `slug`, optional `description`). The calling \
         agent's workspace is auto-added to the space's authorized list."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &SPACES_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&SPACES_OUTPUT_SCHEMA)
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.is_workspace_admin()
            && subject
                .can(AuthVerb::Create, AuthResource::Space(None))
                .is_ok()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: SpacesParams = parse_params(arguments)?;
        Audit::record_action(format!("spaces.{}", params.command_name()));

        let (text, out) = match params {
            SpacesParams::Create { slug, description } => {
                let space = self
                    .spaces
                    .create(subject, slug, description)
                    .await
                    .map_err(|e| ToolSetsError::Space(e.to_string()))?;

                let authorized: Vec<String> = space
                    .authorized_workspaces
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                let text = format!(
                    "Space created.\n  id: {}\n  slug: {}\n  authorized_workspaces: {}",
                    space.id,
                    space.slug,
                    if authorized.is_empty() {
                        "none".to_string()
                    } else {
                        authorized.join(", ")
                    }
                );
                let out = SpacesOutput {
                    command: "create".to_string(),
                    space_id: Some(space.id.to_string()),
                    slug: Some(space.slug.clone()),
                    description: space.description.clone(),
                    authorized_workspaces: Some(authorized),
                };
                (text, out)
            }
        };

        let structured = serde_json::to_value(&out).expect("SpacesOutput serialization");
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        result.structured_content = Some(structured);
        Ok(result)
    }
}
