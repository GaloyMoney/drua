//! `project_sandbox` — consolidated project-scoped sandbox management.
//!
//! Single tool with a `command` discriminator (like `text_editor`):
//! `create`, `list`, `get`, `inspect`.
//!
//! Authorization is delegated entirely to [`Sandboxes`]: each service
//! method runs `subject.can(verb, AuthResource::Sandbox(..))` itself, so
//! this layer only routes parameters and surfaces errors.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::primitives::SandboxId;
use crate::sandbox::{DeleteOutcome, ResizeRequest, Sandbox, Sandboxes};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::parse_params;

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SandboxCommand {
    Create,
    List,
    Get,
    Inspect,
    Restart,
    Suspend,
    Resize,
    Delete,
}

impl SandboxCommand {
    fn audit_action(&self) -> &'static str {
        match self {
            Self::Create => "sandbox.create",
            Self::List => "sandbox.list",
            Self::Get => "sandbox.get",
            Self::Inspect => "sandbox.inspect",
            Self::Restart => "sandbox.restart",
            Self::Suspend => "sandbox.suspend",
            Self::Resize => "sandbox.resize",
            Self::Delete => "sandbox.delete",
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SandboxCreateMode {
    Scratch,
    Repo,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ProjectSandboxParams {
    command: SandboxCommand,

    name: Option<String>,
    mode: Option<SandboxCreateMode>,
    repo_url: Option<String>,
    branch: Option<String>,
    cpu: Option<String>,
    memory: Option<String>,
    disk_size: Option<String>,

    #[schemars(with = "Option<uuid::Uuid>")]
    sandbox_id: Option<SandboxId>,

    /// Inspect sub-op. Each takes a different `op_args` shape:
    /// `ls` → `{ path: string, ignore?: string[] }`;
    /// `read` → `{ path: string, offset?: int, limit?: int }`;
    /// `grep` → `{ pattern: string, path?: string, glob?: string, output_mode?: string }`;
    /// `glob` → `{ pattern: string, path?: string }`.
    op: Option<crate::toolset::inspect::ReadOp>,
    #[serde(default)]
    op_args: Option<JsonObject>,

    /// On `create` and `restart`: block until state=ready (or fail).
    /// Defaults to `true` so callers don't accidentally race a
    /// follow-up call against an in-flight `/initialize`. Pass
    /// `wait: false` only when you genuinely want a fire-and-forget
    /// provisioning. Budget: 6 minutes.
    #[serde(default)]
    wait: Option<bool>,
}

pub struct ProjectSandbox {
    sandboxes: Arc<Sandboxes>,
}

impl ProjectSandbox {
    pub fn new(sandboxes: Arc<Sandboxes>) -> Self {
        Self { sandboxes }
    }
}

static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    let settings = schemars::gen::SchemaSettings::draft07().with(|s| {
        s.inline_subschemas = true;
        s.meta_schema = None;
    });
    let generator = settings.into_generator();
    let schema = generator.into_root_schema_for::<ProjectSandboxParams>();
    let mut value = serde_json::to_value(schema).expect("schema serialization");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("title");
        // `definitions` retained for $ref resolution by the compose TS generator.
        obj.insert(
            "additionalProperties".into(),
            serde_json::Value::Bool(false),
        );
    }
    value
});

#[async_trait::async_trait]
impl TopLevelTool for ProjectSandbox {
    fn name(&self) -> &str {
        "sandbox"
    }

    fn description(&self) -> &str {
        "Manage sandboxes. Commands: `create` (requires `name`, `mode` \
         (`scratch`/`repo`), optional `repo_url`+`branch` for repo mode, \
         plus `cpu`, `memory`, `disk_size`; blocks until state=ready \
         unless `wait: false`), \
         `list`, `get` (requires `sandbox_id`), \
         `inspect` (requires `sandbox_id`, `op` (grep/glob/read/ls), `op_args`; \
         per-op args: ls/read take `path`, grep/glob take `pattern`), \
         `restart` (requires `sandbox_id`; blocks until state=ready unless \
         `wait: false` — wakes a suspended or errored sandbox; cannot run \
         while a sandbox is still provisioning), \
         `suspend` (requires `sandbox_id`), \
         `resize` (requires `sandbox_id` and at least one of `cpu`, `memory`, \
         `disk_size`; disk can only grow, never shrink. CPU/memory changes \
         take effect on the next `restart`; disk growth is applied to the \
         backing PVC immediately), \
         `delete` (requires `sandbox_id`; tears down pod + PVCs and \
         soft-deletes the row — terminal, not the same as `suspend`)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.project_id().is_some_and(|project| {
            subject
                .can(AuthVerb::Read, AuthResource::Sandbox(project, None))
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
        let params: ProjectSandboxParams = parse_params(arguments)?;

        Audit::record_action(params.command.audit_action());

        match params.command {
            SandboxCommand::Create => {
                let name = params.name.ok_or_else(|| {
                    ToolSetsError::MissingArgument("name is required for create".to_string())
                })?;
                let mode_enum = params.mode.ok_or_else(|| {
                    ToolSetsError::MissingArgument("mode is required for create".to_string())
                })?;

                let sandbox_mode = match mode_enum {
                    SandboxCreateMode::Repo => {
                        let repo_url = params.repo_url.ok_or_else(|| {
                            ToolSetsError::InvalidArgument(
                                "repo_url is required when mode is 'repo'".to_string(),
                            )
                        })?;
                        sandbox::SandboxMode::Repo {
                            repo_url,
                            branch: params.branch,
                        }
                    }
                    SandboxCreateMode::Scratch => sandbox::SandboxMode::Scratch,
                };

                let specs = sandbox::SandboxSpecs {
                    cpu: params.cpu.unwrap_or_else(|| "500m".to_string()),
                    memory: params.memory.unwrap_or_else(|| "512Mi".to_string()),
                    disk_size: params.disk_size.unwrap_or_else(|| "10Gi".to_string()),
                };

                let sandbox = self
                    .sandboxes
                    .create(subject, project_id, name, specs, sandbox_mode)
                    .await?;

                if params.wait.unwrap_or(true) {
                    let ready = self
                        .sandboxes
                        .wait_until_ready(sandbox.id, std::time::Duration::from_secs(360))
                        .await?;
                    return Ok(CallToolResult::success(vec![Content::text(
                        format_sandbox(&ready),
                    )]));
                }

                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Sandbox creation requested. Pass `wait: true` (the default) \
                     to block until ready, or poll `get` until state=ready.\n\n{}",
                    format_sandbox(&sandbox)
                ))]))
            }

            SandboxCommand::List => {
                let sandboxes = self.sandboxes.list_for_project(subject, project_id).await?;

                Ok(CallToolResult::success(vec![Content::text(
                    format_sandboxes(&sandboxes),
                )]))
            }

            SandboxCommand::Get => {
                let sandbox_id = params.sandbox_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("sandbox_id is required for get".to_string())
                })?;

                let sandbox = self.sandboxes.find_by_id(subject, sandbox_id).await?;

                Ok(CallToolResult::success(vec![Content::text(
                    format_sandbox(&sandbox),
                )]))
            }

            SandboxCommand::Inspect => {
                let sandbox_id = params.sandbox_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("sandbox_id is required for inspect".to_string())
                })?;
                let op = params.op.ok_or_else(|| {
                    ToolSetsError::MissingArgument("op is required for inspect".to_string())
                })?;
                let op_args = params.op_args.unwrap_or_default();

                Audit::record_sandbox_id(sandbox_id);

                let _ = self.sandboxes.find_by_id(subject, sandbox_id).await?;

                execute_inspect(subject, &self.sandboxes, sandbox_id, op, op_args).await
            }

            SandboxCommand::Restart => {
                let sandbox_id = params.sandbox_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("sandbox_id is required for restart".to_string())
                })?;
                let sandbox = self.sandboxes.restart(subject, sandbox_id).await?;
                if params.wait.unwrap_or(true) {
                    let ready = self
                        .sandboxes
                        .wait_until_ready(sandbox.id, std::time::Duration::from_secs(360))
                        .await?;
                    Ok(CallToolResult::success(vec![Content::text(
                        format_sandbox(&ready),
                    )]))
                } else {
                    Ok(CallToolResult::success(vec![Content::text(format!(
                        "Sandbox restart requested. Pass `wait: true` (the default) to \
                         block until ready, or poll `get` until state=ready.\n\n{}",
                        format_sandbox(&sandbox)
                    ))]))
                }
            }

            SandboxCommand::Suspend => {
                let sandbox_id = params.sandbox_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("sandbox_id is required for suspend".to_string())
                })?;
                let sandbox = self.sandboxes.suspend(subject, sandbox_id).await?;
                Ok(CallToolResult::success(vec![Content::text(
                    format_sandbox(&sandbox),
                )]))
            }

            SandboxCommand::Resize => {
                let sandbox_id = params.sandbox_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("sandbox_id is required for resize".to_string())
                })?;
                // Forward only what the caller actually set; the
                // service merges the rest from an in-transaction load
                // so we don't race a concurrent resize.
                let request = ResizeRequest {
                    cpu: params.cpu,
                    memory: params.memory,
                    disk_size: params.disk_size,
                };
                if request.is_empty() {
                    return Err(ToolSetsError::MissingArgument(
                        "resize: at least one of cpu, memory, disk_size is required".to_string(),
                    ));
                }
                let outcome = self.sandboxes.resize(subject, sandbox_id, request).await?;
                let header = match &outcome.pvc_warning {
                    None => "Sandbox specs updated. CPU/memory changes apply on the \
                             next `restart`; any disk grow was patched on the workspace PVC."
                        .to_string(),
                    Some(w) => format!(
                        "Sandbox specs updated, but the workspace-PVC patch failed: {w}\n\
                         The DB now records the new disk size; a follow-up resize \
                         (or manual PVC reconcile) is needed to grow the cluster volume."
                    ),
                };
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "{header}\n\n{}",
                    format_sandbox(&outcome.sandbox)
                ))]))
            }

            SandboxCommand::Delete => {
                let sandbox_id = params.sandbox_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("sandbox_id is required for delete".to_string())
                })?;
                let outcome = self.sandboxes.delete(subject, sandbox_id).await?;
                Ok(CallToolResult::success(vec![Content::text(
                    format_delete_outcome(sandbox_id, &outcome),
                )]))
            }
        }
    }
}

async fn execute_inspect(
    sub: &AuthSubject,
    sandboxes: &Sandboxes,
    sandbox_id: SandboxId,
    op: crate::toolset::inspect::ReadOp,
    op_args: JsonObject,
) -> Result<CallToolResult, ToolSetsError> {
    use crate::toolset::inspect::ReadOp;
    let is_ls = matches!(op, ReadOp::Ls);

    // Extract LS ignore list before op_args is moved.
    let ls_ignore: Vec<String> = if is_ls {
        op_args
            .get("ignore")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let req = match op {
        ReadOp::Grep => ExecuteRequest {
            tool: "Grep".to_string(),
            input: serde_json::Value::Object(op_args),
        },
        ReadOp::Glob => ExecuteRequest {
            tool: "Glob".to_string(),
            input: serde_json::Value::Object(op_args),
        },
        ReadOp::Read => build_read_request(&op_args)?,
        ReadOp::Ls => build_ls_request(&op_args)?,
    };

    let client = sandboxes.instance_client_for_read(sub, sandbox_id).await?;

    match client.execute(&req).await {
        Ok(resp) => {
            let mut output = resp.output;

            if is_ls && !ls_ignore.is_empty() {
                output = output
                    .lines()
                    .filter(|line| {
                        let name = line.trim_end_matches('/');
                        !ls_ignore.iter().any(|ig| ig == name)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
            }

            let content = vec![Content::text(output)];
            if resp.is_error {
                Ok(CallToolResult::error(content))
            } else {
                Ok(CallToolResult::success(content))
            }
        }
        Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
            "sandbox /execute call failed: {e}"
        ))])),
    }
}

fn build_read_request(args: &JsonObject) -> Result<ExecuteRequest, ToolSetsError> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolSetsError::MissingArgument("path".to_string()))?;

    let mut input = serde_json::json!({
        "command": "view",
        "path": path,
    });

    if let Some((start, end)) = super::super::inspect::parse_view_range(args) {
        input["view_range"] = serde_json::json!([start, end]);
    }

    Ok(ExecuteRequest {
        tool: "str_replace_based_edit_tool".to_string(),
        input,
    })
}

fn build_ls_request(args: &JsonObject) -> Result<ExecuteRequest, ToolSetsError> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolSetsError::MissingArgument("path".to_string()))?;

    Ok(ExecuteRequest {
        tool: "str_replace_based_edit_tool".to_string(),
        input: serde_json::json!({
            "command": "view",
            "path": path,
        }),
    })
}

fn format_sandbox(s: &Sandbox) -> String {
    let agents: Vec<String> = s
        .attached_agents
        .iter()
        .map(|(id, mode)| format!("  {id} ({mode:?})"))
        .collect();
    let agents_str = if agents.is_empty() {
        "  none".to_string()
    } else {
        agents.join("\n")
    };
    let error_str = s
        .last_error
        .as_deref()
        .map(|e| format!("\n  last_error: {e}"))
        .unwrap_or_default();
    format!(
        "Sandbox:\n  id: {}\n  name: {}\n  project: {}\n  state: {}\n  mode: {:?}\n  specs: cpu={}, mem={}, disk={}{}\n  attached_agents:\n{}",
        s.id, s.name, s.project_id, s.state, s.mode,
        s.specs.cpu, s.specs.memory, s.specs.disk_size,
        error_str, agents_str,
    )
}

/// Render a per-step admin-teardown report so the caller doesn't have
/// to guess whether the pod / PVCs actually went away — bugbot caught
/// the original "Pod + PVCs torn down" claim being asserted even when
/// the admin calls had failed and only logged a warning.
fn format_delete_outcome(sandbox_id: SandboxId, outcome: &DeleteOutcome) -> String {
    let header = format!("Sandbox {sandbox_id}: row soft-deleted and no longer addressable.");
    if outcome.admin_teardown_clean() {
        return format!("{header}\nPod + PVC teardown: ok.");
    }
    let pod_line = match &outcome.pod_warning {
        None => "Pod teardown: ok.".to_string(),
        Some(w) => format!("Pod teardown FAILED: {w}"),
    };
    let pvc_line = match &outcome.pvcs_warning {
        None => "PVC teardown: ok.".to_string(),
        Some(w) => format!("PVC teardown FAILED: {w}"),
    };
    format!(
        "{header}\n{pod_line}\n{pvc_line}\n\
         The row is gone; the leaked cluster resources can be reconciled \
         out-of-band (k8s objects are labeled `sandbox-name=<resource_name>`)."
    )
}

fn format_sandboxes(sandboxes: &[Sandbox]) -> String {
    if sandboxes.is_empty() {
        return "No sandboxes found.".to_string();
    }

    let mut lines = Vec::with_capacity(sandboxes.len() + 2);
    lines.push(format!(
        "{:<38} {:<20} {:<14} {:<10} {:<8}",
        "ID", "NAME", "STATE", "MODE", "AGENTS"
    ));
    lines.push("-".repeat(94));

    for s in sandboxes {
        let mode = format!("{:?}", s.mode);
        lines.push(format!(
            "{:<38} {:<20} {:<14} {:<10} {:<8}",
            s.id,
            truncate(&s.name, 20),
            s.state.to_string(),
            truncate(&mode, 10),
            s.attached_agents.len(),
        ));
    }

    lines.join("\n")
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
