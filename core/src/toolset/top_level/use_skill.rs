use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::agent::{AgentWorkflowContext, Agents, WorkflowStepInvocation};
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::skill::{SkillBody, Skills};
use crate::workflow::run::WorkflowRunRepo;
use crate::workflow::template::{contains_template_ref, render_workflow_skill, TemplateContext};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;

fn default_search_limit() -> usize {
    10
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum UseSkillAction {
    Invoke {
        name: String,
        #[serde(default)]
        arguments: Option<String>,
    },
    Search {
        #[serde(default)]
        query: String,
        #[serde(default = "default_search_limit")]
        limit: usize,
    },
}

/// Wrapper that defaults to Search when `action` is missing.
fn parse_use_skill_params(arguments: Option<JsonObject>) -> Result<UseSkillAction, ToolSetsError> {
    let Some(args) = arguments else {
        return Ok(UseSkillAction::Search {
            query: String::new(),
            limit: default_search_limit(),
        });
    };
    let map: serde_json::Map<String, serde_json::Value> = args.into_iter().collect();
    if map.contains_key("action") {
        let val = serde_json::Value::Object(map);
        serde_json::from_value(val).map_err(|e| ToolSetsError::Skill(e.to_string()))
    } else if let Some(name) = map.get("name").and_then(|v| v.as_str()) {
        Ok(UseSkillAction::Invoke {
            name: name.to_string(),
            arguments: map
                .get("arguments")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    } else {
        Ok(UseSkillAction::Search {
            query: map
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            limit: map
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(default_search_limit() as u64) as usize,
        })
    }
}

static USE_SKILL_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["invoke", "search"],
                "description": "Which operation to perform: invoke a skill by name, or search for skills by topic."
            },
            "name": {
                "type": "string",
                "description": "Skill name to invoke (invoke action)."
            },
            "arguments": {
                "type": "string",
                "description": "Arguments to pass to the skill. Replaces $ARGUMENTS in the skill body (invoke action, optional)."
            },
            "query": {
                "type": "string",
                "description": "Search query — keywords or natural language (search action)."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "description": "Maximum number of search results (search action, default 10)."
            }
        },
        "required": ["action"],
        "additionalProperties": false
    })
});

pub struct UseSkillTool {
    skills: Arc<Skills>,
    agents: Arc<Agents>,
    runs: WorkflowRunRepo,
}

impl UseSkillTool {
    pub fn new(skills: Arc<Skills>, agents: Arc<Agents>, runs: WorkflowRunRepo) -> Self {
        Self {
            skills,
            agents,
            runs,
        }
    }

    /// Replays the assigned skill's original expansion verbatim, from
    /// the step agent's own session — the authoritative step-start
    /// snapshot. Never re-renders: a mid-run library edit or a
    /// currently-different set of step outputs must not change what
    /// comes back.
    async fn replay_assigned_skill(
        &self,
        inv: &WorkflowStepInvocation,
    ) -> Result<CallToolResult, ToolSetsError> {
        let original = self
            .agents
            .first_user_input(inv.agent_id)
            .await
            .map_err(|e| ToolSetsError::Skill(e.to_string()))?;
        match original {
            Some(text) => Ok(CallToolResult::success(vec![Content::text(text)])),
            None => Err(ToolSetsError::Skill(format!(
                "workflow step '{}' has no recoverable initial prompt to replay for skill '{}'",
                inv.step_name, inv.assigned_skill,
            ))),
        }
    }

    /// Renders `raw_body` against this step's trusted context: the
    /// run's original trigger, the previous outputs supplied to this
    /// step, and the run's own identity — reloaded fresh from the
    /// run's own persisted, append-only state (never from caller-
    /// supplied arguments) so it can't be redirected to another run.
    async fn render_in_workflow(
        &self,
        inv: &WorkflowStepInvocation,
        raw_body: &str,
        arguments: Option<&str>,
    ) -> Result<String, ToolSetsError> {
        let run = self
            .runs
            .find_by_id(inv.workflow_run_id)
            .await
            .map_err(|e| ToolSetsError::Skill(e.to_string()))?;
        let step_outputs = run.step_outputs_snapshot();
        let run_context = run.base_run_context();
        let template_ctx = TemplateContext {
            trigger: &run.trigger_context,
            steps: &step_outputs,
            run: &run_context,
        };
        render_workflow_skill(raw_body, &template_ctx, arguments)
            .map_err(|e| ToolSetsError::Skill(e.to_string()))
    }
}

#[async_trait::async_trait]
impl TopLevelTool for UseSkillTool {
    fn name(&self) -> &str {
        "use_skill"
    }

    fn description(&self) -> &str {
        "Invoke or search project skills. Use action \"invoke\" with a skill \
         name to load the full skill body (with optional $ARGUMENTS substitution). \
         Use action \"search\" with a query to discover skills by topic."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &USE_SKILL_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.project_id().is_some_and(|project| {
            subject
                .can(AuthVerb::Use, AuthResource::Skill(project, None))
                .is_ok()
        })
    }

    fn composable(&self) -> bool {
        false
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params = parse_use_skill_params(arguments)?;

        match params {
            UseSkillAction::Invoke { name, arguments } => {
                let project_id = subject.project_id();
                let sandbox_id = subject.readable_sandbox_id();

                // Resolved entirely from the authenticated agent's own
                // persisted state (never from `name`/`arguments`, which
                // stay caller-controlled) — see `Agents::workflow_context`.
                let workflow_ctx = match subject.acting_agent_id() {
                    Some(agent_id) => self
                        .agents
                        .workflow_context(agent_id)
                        .await
                        .map_err(|e| ToolSetsError::Skill(e.to_string()))?,
                    None => AgentWorkflowContext::None,
                };

                // Fast path: the assigned skill, re-invoked with no new
                // arguments, replays its original expansion verbatim
                // instead of re-rendering (stable across library edits
                // and workflow resume/retry).
                if let AgentWorkflowContext::Invocation(inv) = &workflow_ctx {
                    if arguments.is_none() && name == inv.assigned_skill {
                        return self.replay_assigned_skill(inv).await;
                    }
                }

                // `Skills::find_by_name` resolves mounted-space skills
                // internally via its held `SpaceMounts` — callers just
                // pass project + sandbox. Fetched here (rather than via
                // `Skills::interpolate_skill`) so the workflow branches
                // below can render the same raw body against a trusted
                // `TemplateContext` before any `$ARGUMENTS` substitution.
                let raw_body = self
                    .skills
                    .find_by_name(&name, project_id, sandbox_id)
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;
                let Some(raw_body) = raw_body else {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Unknown skill: {name}"
                    ))]));
                };
                let raw_body: String = raw_body.into();

                match &workflow_ctx {
                    // Another authorized skill (or the assigned skill
                    // with new arguments): resolve via the existing
                    // project/space/sandbox precedence above, then render
                    // workflow references against this step's trusted
                    // context. Explicit `arguments` keep their ordinary
                    // $ARGUMENTS/positional semantics; they cannot select
                    // another run or replace its trigger/step namespace.
                    AgentWorkflowContext::Invocation(inv) => {
                        let rendered = self
                            .render_in_workflow(inv, &raw_body, arguments.as_deref())
                            .await?;
                        Ok(CallToolResult::success(vec![Content::text(rendered)]))
                    }
                    // Workflow agent with no recoverable step/skill
                    // association (predates this tracking). A skill with
                    // no workflow template syntax is harmless to hand
                    // back as-is; one that needs workflow context must
                    // not be returned unresolved as if it were a
                    // successful invocation.
                    AgentWorkflowContext::Legacy if contains_template_ref(&raw_body) => {
                        Ok(CallToolResult::error(vec![Content::text(format!(
                            "Skill '{name}' contains workflow template expressions, but this \
                             agent has no recoverable step context to resolve them against \
                             (a workflow run predating this tracking). Use the instructions \
                             already expanded in this conversation instead of reloading them."
                        ))]))
                    }
                    AgentWorkflowContext::Legacy | AgentWorkflowContext::None => {
                        let rendered = SkillBody::new(raw_body).interpolate(arguments.as_deref());
                        Ok(CallToolResult::success(vec![Content::text(rendered)]))
                    }
                }
            }

            UseSkillAction::Search { query, limit } => {
                let project_id = subject.project_id().ok_or(ToolSetsError::Unauthorized)?;
                let results = self
                    .skills
                    .search(subject, project_id, &query, limit)
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;

                if results.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No skills found matching your query.",
                    )]));
                }

                let text = results
                    .iter()
                    .map(|r| format!("- {} — {}", r.fields.name, truncate(&r.fields.content, 200)))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Found {} skill(s):\n\n{text}",
                    results.len(),
                ))]))
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}...")
    }
}
