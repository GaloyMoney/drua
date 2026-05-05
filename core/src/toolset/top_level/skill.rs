//! Skill *management*. [`super::use_skill::UseSkillTool`] is the
//! invocation surface.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::primitives::SkillId;
use crate::project::Projects;
use crate::skill::{Skill, Skills};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum SkillParams {
    Create {
        name: String,
        description: String,
        body: String,
    },
    Update {
        skill_id: SkillId,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        body: Option<String>,
    },
    Delete {
        skill_id: SkillId,
    },
    List,
    Get {
        skill_id: SkillId,
    },
}

impl SkillParams {
    fn audit_action(&self) -> &'static str {
        match self {
            Self::Create { .. } => "skill.create",
            Self::Update { .. } => "skill.update",
            Self::Delete { .. } => "skill.delete",
            Self::List => "skill.list",
            Self::Get { .. } => "skill.get",
        }
    }
}

#[derive(Default, serde::Serialize, schemars::JsonSchema)]
struct SkillOutput {
    /// Which command was executed.
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    skill: Option<SkillSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills: Option<Vec<SkillSummary>>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct SkillSummary {
    id: String,
    name: String,
    description: String,
    /// `"project"` or `"global"`.
    scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    /// Set on `get` / `create` / `update`; omitted from `list`.
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
}

pub struct SkillTool {
    skills: Arc<Skills>,
    projects: Arc<Projects>,
}

impl SkillTool {
    pub fn new(skills: Arc<Skills>, projects: Arc<Projects>) -> Self {
        Self { skills, projects }
    }
}

static SKILL_OUTPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<SkillOutput>);

static SKILL_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "enum": ["create", "update", "delete", "list", "get"],
                "description": "Which skill operation to perform."
            },
            "name": {
                "type": "string",
                "description": "Skill name (required for create; optional for update)."
            },
            "description": {
                "type": "string",
                "description": "One-line description of what the skill does (required for create; optional for update)."
            },
            "body": {
                "type": "string",
                "description": "Skill body — the markdown / prompt template that gets resolved when the skill is invoked. Use $ARGUMENTS / $0 / $1 / … placeholders for runtime substitution. Required for create; optional for update."
            },
            "skill_id": {
                "type": "string",
                "format": "uuid",
                "description": "Skill ID. Required for update / delete / get."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
});

#[async_trait::async_trait]
impl TopLevelTool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Manage project-scoped skills. A skill BODY is the natural-language \
         prompt fed to the agent that runs the step / `use_skill` call — \
         describe the GOAL, not shell scripts. The agent that runs a skill \
         has these tools: `bash`, `text_editor` (write/read/replace), `ls`, \
         `grep`, `glob`, `read`, plus any sandbox attached (sandbox-backed \
         tools operate inside the sandbox filesystem). Use `$ARGUMENTS`, \
         `$0`, `$1`, … to interpolate trigger inputs; if the body has no \
         placeholder, arguments are appended as `ARGUMENTS: <value>`. \
         Commands: `create` (requires `name`, `description`, `body`), \
         `update` (requires `skill_id`; any of `name`/`description`/`body`), \
         `delete` (requires `skill_id`; project- or global-scoped skills only \
         — for space-scoped skills use the `spaces` tool), \
         `list`, `get` (requires `skill_id`)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &SKILL_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&SKILL_OUTPUT_SCHEMA)
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        // Management surface: visible iff subject can mutate skills (admin scope).
        subject.project_id().is_some_and(|project| {
            subject
                .can(AuthVerb::Update, AuthResource::Skill(project, None))
                .is_ok()
        })
    }

    fn composable(&self) -> bool {
        true
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let project_id = subject.project_id().ok_or(ToolSetsError::Unauthorized)?;
        let params: SkillParams = parse_params(arguments)?;

        Audit::record_action(params.audit_action());

        let (text, out) = match params {
            SkillParams::Create {
                name,
                description,
                body,
            } => {
                let project_name = self
                    .projects
                    .find_by_id(subject, project_id)
                    .await
                    .map(|w| w.name)?;

                let skill = self
                    .skills
                    .create(subject, project_id, &project_name, name, description, body)
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;

                let text = format!(
                    "Skill created.\n  id:   {}\n  name: {}",
                    skill.id, skill.name
                );
                let out = SkillOutput {
                    command: "create".to_string(),
                    skill: Some(skill_to_summary(&skill, true)),
                    ..Default::default()
                };
                (text, out)
            }

            SkillParams::Update {
                skill_id,
                name,
                description,
                body,
            } => {
                let skill = self
                    .skills
                    .update(subject, skill_id, project_id, name, description, body)
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;

                let text = format!(
                    "Skill updated.\n  id:   {}\n  name: {}",
                    skill.id, skill.name
                );
                let out = SkillOutput {
                    command: "update".to_string(),
                    skill: Some(skill_to_summary(&skill, true)),
                    ..Default::default()
                };
                (text, out)
            }

            SkillParams::Delete { skill_id } => {
                self.skills
                    .delete(subject, skill_id, project_id)
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;

                let text = format!("Skill deleted (id {skill_id}).");
                let out = SkillOutput {
                    command: "delete".to_string(),
                    ..Default::default()
                };
                (text, out)
            }

            SkillParams::List => {
                let skills = self
                    .skills
                    .list_for_project(subject, project_id)
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;
                let text = format_list_text(&skills);
                let out = SkillOutput {
                    command: "list".to_string(),
                    skills: Some(skills.iter().map(|s| skill_to_summary(s, false)).collect()),
                    ..Default::default()
                };
                (text, out)
            }

            SkillParams::Get { skill_id } => {
                let skill = self
                    .skills
                    .find_by_id(subject, skill_id, project_id)
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;
                let text = format_get_text(&skill);
                let out = SkillOutput {
                    command: "get".to_string(),
                    skill: Some(skill_to_summary(&skill, true)),
                    ..Default::default()
                };
                (text, out)
            }
        };

        let structured = serde_json::to_value(&out).expect("SkillOutput serialization");
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        result.structured_content = Some(structured);
        Ok(result)
    }
}

fn skill_to_summary(s: &Skill, include_body: bool) -> SkillSummary {
    let scope = if s.project_id.is_some() {
        "project"
    } else {
        "global"
    };
    SkillSummary {
        id: s.id.to_string(),
        name: s.name.clone(),
        description: s.description.clone(),
        scope: scope.to_string(),
        project_id: s.project_id.map(|w| w.to_string()),
        body: include_body.then(|| s.body.clone()),
    }
}

fn format_list_text(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return "No skills found.".to_string();
    }
    let mut lines = Vec::with_capacity(skills.len() + 2);
    lines.push(format!(
        "{:<38} {:<24} {:<10} {}",
        "ID", "NAME", "SCOPE", "DESCRIPTION"
    ));
    lines.push("-".repeat(110));
    for s in skills {
        let scope = if s.project_id.is_some() {
            "project"
        } else {
            "global"
        };
        lines.push(format!(
            "{:<38} {:<24} {:<10} {}",
            s.id,
            truncate(&s.name, 24),
            scope,
            truncate(&s.description, 50)
        ));
    }
    lines.join("\n")
}

fn format_get_text(s: &Skill) -> String {
    let scope = if s.project_id.is_some() {
        "project"
    } else {
        "global"
    };
    format!(
        "id:          {}\nname:        {}\nscope:       {}\ndescription: {}\n\n--- body ---\n{}",
        s.id, s.name, scope, s.description, s.body
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}
