//! YAML wire-format for `WorkflowDefinition`. Lives here (not in
//! `library/`) because the schema is workflow-specific; the library
//! only owns transport (`WriteOp`).

use llm::ModelChain;

use crate::primitives::WorkflowDefinitionId;
use crate::sandbox::{SandboxAgentMode, SandboxMode, SandboxSpecs};

use super::definition::{default_output_schema, OutputSchema};
use crate::skill::file::slugify;
use crate::skill::name_from_filename;

use super::definition::{WorkflowSandboxDecl, WorkflowStepDef, WorkflowTrigger};

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct WorkflowYaml {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<uuid::Uuid>,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    trigger: WorkflowTriggerYaml,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_chain: Option<ModelChain>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sandboxes: Vec<WorkflowSandboxYaml>,
    steps: Vec<WorkflowStepYaml>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    created: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    updated: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkflowSandboxYaml {
    Scratch {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<ScratchYamlConfig>,
    },
    Repo {
        name: String,
        config: RepoYamlConfig,
    },
    /// References an existing sandbox in the workflow's project by
    /// name (project-unique). The workflow executor only attaches —
    /// no provisioning, no lifecycle management. No `config` block:
    /// the sandbox already has its own.
    Preexisting { name: String },
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct ScratchYamlConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cpu: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disk_size: Option<String>,
}

impl ScratchYamlConfig {
    fn into_specs(self) -> Option<SandboxSpecs> {
        match (self.cpu, self.memory, self.disk_size) {
            (Some(cpu), Some(memory), Some(disk_size)) => Some(SandboxSpecs {
                cpu,
                memory,
                disk_size,
            }),
            _ => None,
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct RepoYamlConfig {
    repo_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cpu: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disk_size: Option<String>,
}

impl RepoYamlConfig {
    fn split(self) -> (SandboxMode, Option<SandboxSpecs>) {
        let mode = SandboxMode::Repo {
            repo_url: self.repo_url,
            branch: self.branch,
        };
        let specs = match (self.cpu, self.memory, self.disk_size) {
            (Some(cpu), Some(memory), Some(disk_size)) => Some(SandboxSpecs {
                cpu,
                memory,
                disk_size,
            }),
            _ => None,
        };
        (mode, specs)
    }
}

impl WorkflowSandboxYaml {
    fn from_runtime(d: &WorkflowSandboxDecl) -> Self {
        match d {
            WorkflowSandboxDecl::Preexisting { name } => {
                WorkflowSandboxYaml::Preexisting { name: name.clone() }
            }
            WorkflowSandboxDecl::Provisioned { name, mode, specs } => {
                let (cpu, memory, disk_size) = match specs {
                    Some(s) => (
                        Some(s.cpu.clone()),
                        Some(s.memory.clone()),
                        Some(s.disk_size.clone()),
                    ),
                    None => (None, None, None),
                };
                match mode {
                    SandboxMode::Scratch => {
                        let config = if cpu.is_some() || memory.is_some() || disk_size.is_some() {
                            Some(ScratchYamlConfig {
                                cpu,
                                memory,
                                disk_size,
                            })
                        } else {
                            None
                        };
                        WorkflowSandboxYaml::Scratch {
                            name: name.clone(),
                            config,
                        }
                    }
                    SandboxMode::Repo { repo_url, branch } => WorkflowSandboxYaml::Repo {
                        name: name.clone(),
                        config: RepoYamlConfig {
                            repo_url: repo_url.clone(),
                            branch: branch.clone(),
                            cpu,
                            memory,
                            disk_size,
                        },
                    },
                }
            }
        }
    }

    fn into_runtime(self) -> WorkflowSandboxDecl {
        match self {
            WorkflowSandboxYaml::Preexisting { name } => WorkflowSandboxDecl::Preexisting { name },
            WorkflowSandboxYaml::Scratch { name, config } => WorkflowSandboxDecl::Provisioned {
                name,
                mode: SandboxMode::Scratch,
                specs: config.unwrap_or_default().into_specs(),
            },
            WorkflowSandboxYaml::Repo { name, config } => {
                let (mode, specs) = config.split();
                WorkflowSandboxDecl::Provisioned { name, mode, specs }
            }
        }
    }
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkflowTriggerYaml {
    #[default]
    Manual,
    Webhook {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    Cron {
        schedule: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
}

impl WorkflowTriggerYaml {
    fn from_runtime(t: &WorkflowTrigger) -> Self {
        match t {
            WorkflowTrigger::Manual => WorkflowTriggerYaml::Manual,
            WorkflowTrigger::Webhook { provider, .. } => WorkflowTriggerYaml::Webhook {
                provider: provider.clone(),
            },
            WorkflowTrigger::Cron { schedule, timezone } => WorkflowTriggerYaml::Cron {
                schedule: schedule.clone(),
                timezone: timezone.clone(),
            },
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkflowStepYaml {
    AgentStep {
        name: String,
        skill: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sandbox: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sandbox_mode: Option<SandboxAgentMode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_seconds: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_chain: Option<ModelChain>,
        #[serde(default = "default_output_schema")]
        output_schema: OutputSchema,
    },
}

impl WorkflowStepYaml {
    fn from_runtime(s: &WorkflowStepDef) -> Self {
        match s {
            WorkflowStepDef::AgentStep {
                name,
                skill,
                sandbox,
                sandbox_mode,
                timeout_seconds,
                model_chain,
                output_schema,
            } => WorkflowStepYaml::AgentStep {
                name: name.clone(),
                skill: skill.clone(),
                sandbox: sandbox.clone(),
                sandbox_mode: *sandbox_mode,
                timeout_seconds: *timeout_seconds,
                model_chain: model_chain.clone(),
                output_schema: output_schema.clone(),
            },
        }
    }

    fn into_runtime(self) -> WorkflowStepDef {
        match self {
            WorkflowStepYaml::AgentStep {
                name,
                skill,
                sandbox,
                sandbox_mode,
                timeout_seconds,
                model_chain,
                output_schema,
            } => WorkflowStepDef::AgentStep {
                name,
                skill,
                sandbox,
                sandbox_mode,
                timeout_seconds,
                model_chain,
                output_schema,
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render_workflow_yaml(
    doc_id: WorkflowDefinitionId,
    name: &str,
    description: Option<&str>,
    trigger: &WorkflowTrigger,
    steps: &[WorkflowStepDef],
    sandboxes: &[WorkflowSandboxDecl],
    model_chain: Option<&ModelChain>,
    created_at: &str,
    updated_at: &str,
) -> String {
    let yaml = WorkflowYaml {
        id: Some(doc_id.into()),
        name: name.to_string(),
        description: description.map(|s| s.to_string()),
        trigger: WorkflowTriggerYaml::from_runtime(trigger),
        model_chain: model_chain.cloned(),
        sandboxes: sandboxes
            .iter()
            .map(WorkflowSandboxYaml::from_runtime)
            .collect(),
        steps: steps.iter().map(WorkflowStepYaml::from_runtime).collect(),
        created: created_at.to_string(),
        updated: updated_at.to_string(),
    };
    serde_yaml::to_string(&yaml).unwrap_or_else(|e| format!("# yaml render error: {e}\n"))
}

/// Flat result of parsing a workflow YAML file from the library.
/// Replaces the old `(SyncedFile, ParsedFile)` pair the entity-import
/// pipeline used.
pub struct ParsedWorkflow {
    pub workflow_id: WorkflowDefinitionId,
    pub project_name: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub trigger: WorkflowTrigger,
    pub steps: Vec<WorkflowStepDef>,
    pub sandboxes: Vec<WorkflowSandboxDecl>,
    pub model_chain: Option<ModelChain>,
    pub created_at: String,
    pub updated_at: String,
    pub original_path: String,
    pub rendered: String,
    /// True when the on-disk form is non-canonical (no `id:`, stale path, …)
    /// and the importer should re-render to canonical form.
    pub needs_rewrite: bool,
}

/// `None` when the YAML is malformed or no name can be derived.
pub fn parse_workflow_yaml(content: &str, path: &str) -> Option<ParsedWorkflow> {
    let project_name = project_name_from_workflow_path(path);
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    let yaml: WorkflowYaml = serde_yaml::from_str(trimmed).ok()?;

    let (workflow_id, has_id) = match yaml.id {
        Some(uuid) => (WorkflowDefinitionId::from(uuid), true),
        None => (WorkflowDefinitionId::new(), false),
    };

    let name = if !yaml.name.is_empty() {
        yaml.name
    } else {
        name_from_filename(path)?
    };

    let trigger = match yaml.trigger {
        WorkflowTriggerYaml::Manual => WorkflowTrigger::Manual,
        WorkflowTriggerYaml::Webhook { provider } => WorkflowTrigger::Webhook {
            provider,
            secret: String::new(),
        },
        WorkflowTriggerYaml::Cron { schedule, timezone } => {
            WorkflowTrigger::Cron { schedule, timezone }
        }
    };

    let steps: Vec<WorkflowStepDef> = yaml
        .steps
        .into_iter()
        .map(WorkflowStepYaml::into_runtime)
        .collect();
    let sandboxes: Vec<WorkflowSandboxDecl> = yaml
        .sandboxes
        .into_iter()
        .map(WorkflowSandboxYaml::into_runtime)
        .collect();

    let description = yaml.description;
    let model_chain = yaml.model_chain;

    let rendered = render_workflow_yaml(
        workflow_id,
        &name,
        description.as_deref(),
        &trigger,
        &steps,
        &sandboxes,
        model_chain.as_ref(),
        &yaml.created,
        &yaml.updated,
    );

    let canonical_path = canonical_workflow_path(&name, project_name.as_deref());
    let needs_rewrite = !has_id || canonical_path != path;

    Some(ParsedWorkflow {
        workflow_id,
        project_name,
        name,
        description,
        trigger,
        steps,
        sandboxes,
        model_chain,
        created_at: yaml.created,
        updated_at: yaml.updated,
        original_path: path.to_string(),
        rendered,
        needs_rewrite,
    })
}

pub fn canonical_workflow_path(name: &str, project_name: Option<&str>) -> String {
    let slug = slugify(name);
    match project_name {
        Some(project) => format!("runtime/projects/{project}/workflows/{slug}.yml"),
        None => format!("runtime/workflows/{slug}.yml"),
    }
}

/// `runtime/projects/{project}/workflows/*.yml` → `Some(project)`;
/// `runtime/workflows/*.yml` → `None` (global).
pub fn project_name_from_workflow_path(relative_path: &str) -> Option<String> {
    let parts: Vec<&str> = relative_path.split('/').collect();
    if parts.len() >= 5
        && parts[0] == "runtime"
        && parts[1] == "projects"
        && parts[3] == "workflows"
    {
        Some(parts[2].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_steps() -> Vec<WorkflowStepDef> {
        vec![WorkflowStepDef::AgentStep {
            name: "investigate".to_string(),
            skill: "alert-investigator".to_string(),
            sandbox: Some("investigation".to_string()),
            sandbox_mode: None,
            timeout_seconds: Some(120),
            model_chain: None,
            output_schema: default_output_schema(),
        }]
    }

    fn sample_sandboxes() -> Vec<WorkflowSandboxDecl> {
        vec![WorkflowSandboxDecl::Provisioned {
            name: "investigation".to_string(),
            mode: SandboxMode::Scratch,
            specs: None,
        }]
    }

    fn render(
        id: WorkflowDefinitionId,
        name: &str,
        description: Option<&str>,
        trigger: &WorkflowTrigger,
        sandboxes: &[WorkflowSandboxDecl],
    ) -> String {
        render_workflow_yaml(
            id,
            name,
            description,
            trigger,
            &sample_steps(),
            sandboxes,
            None,
            "2026-04-29T00:00:00Z",
            "2026-04-29T00:00:00Z",
        )
    }

    #[test]
    fn workflow_yaml_roundtrip_global() {
        let id = WorkflowDefinitionId::new();
        let trigger = WorkflowTrigger::Webhook {
            provider: Some("honeycomb".to_string()),
            secret: "whsec_should-not-be-serialized".to_string(),
        };
        let content = render(
            id,
            "alert-response",
            Some("Investigate Honeycomb alerts"),
            &trigger,
            &sample_sandboxes(),
        );
        assert!(!content.contains("whsec_should-not-be-serialized"));

        let path = canonical_workflow_path("alert-response", None);
        let parsed = parse_workflow_yaml(&content, &path).expect("parses");
        assert!(!parsed.needs_rewrite);

        assert_eq!(parsed.workflow_id, id);
        assert_eq!(parsed.project_name, None);
        assert_eq!(parsed.name, "alert-response");
        assert_eq!(
            parsed.description.as_deref(),
            Some("Investigate Honeycomb alerts")
        );
        match parsed.trigger {
            WorkflowTrigger::Webhook {
                ref provider,
                ref secret,
            } => {
                assert_eq!(provider.as_deref(), Some("honeycomb"));
                assert_eq!(secret, "");
            }
            _ => panic!("expected webhook trigger"),
        }
        assert_eq!(parsed.steps.len(), 1);
    }

    #[test]
    fn workflow_yaml_roundtrip_preserves_sandboxes() {
        let id = WorkflowDefinitionId::new();
        let content = render(
            id,
            "alert-response",
            None,
            &WorkflowTrigger::Manual,
            &sample_sandboxes(),
        );
        let path = canonical_workflow_path("alert-response", None);
        let parsed = parse_workflow_yaml(&content, &path).expect("parses");
        assert_eq!(parsed.sandboxes.len(), 1);
        assert_eq!(parsed.sandboxes[0].name(), "investigation");
        assert!(matches!(
            parsed.sandboxes[0],
            WorkflowSandboxDecl::Provisioned {
                mode: SandboxMode::Scratch,
                ..
            }
        ));
    }

    #[test]
    fn workflow_yaml_project_scoped_path() {
        let path = "runtime/projects/team/workflows/foo.yml";
        assert_eq!(
            project_name_from_workflow_path(path),
            Some("team".to_string())
        );
    }

    #[test]
    fn canonical_workflow_path_omits_id_suffix() {
        assert_eq!(
            canonical_workflow_path("Hello World Workflow", Some("test")),
            "runtime/projects/test/workflows/hello-world-workflow.yml"
        );
        assert_eq!(
            canonical_workflow_path("alert-response", None),
            "runtime/workflows/alert-response.yml"
        );
    }

    #[test]
    fn workflow_yaml_without_id_needs_rewrite() {
        let content = "\
name: simple-flow
trigger:
  type: manual
steps:
  - type: agent_step
    name: step
    skill: my-skill
";
        let path = "runtime/workflows/simple-flow.yml";
        let parsed = parse_workflow_yaml(content, path).expect("parses");
        assert!(parsed.needs_rewrite);
        assert_eq!(parsed.name, "simple-flow");
        assert!(matches!(parsed.trigger, WorkflowTrigger::Manual));
    }

    #[test]
    fn workflow_yaml_roundtrip_cron_trigger() {
        let id = WorkflowDefinitionId::new();
        let trigger = WorkflowTrigger::Cron {
            schedule: "0 */6 * * * *".to_string(),
            timezone: Some("America/New_York".to_string()),
        };
        let content = render(id, "scheduled", None, &trigger, &sample_sandboxes());
        assert!(content.contains("type: cron"));
        assert!(content.contains("schedule:"));
        assert!(content.contains("timezone: America/New_York"));

        let path = canonical_workflow_path("scheduled", None);
        let parsed = parse_workflow_yaml(&content, &path).expect("parses");
        match parsed.trigger {
            WorkflowTrigger::Cron { schedule, timezone } => {
                assert_eq!(schedule, "0 */6 * * * *");
                assert_eq!(timezone.as_deref(), Some("America/New_York"));
            }
            _ => panic!("expected cron trigger"),
        }
    }

    #[test]
    fn workflow_yaml_roundtrip_cron_trigger_default_timezone() {
        let content = "\
name: scheduled
trigger:
  type: cron
  schedule: \"0 */6 * * * *\"
steps:
  - type: agent_step
    name: step
    skill: my-skill
";
        let path = "runtime/workflows/scheduled.yml";
        let parsed = parse_workflow_yaml(content, path).expect("parses");
        match parsed.trigger {
            WorkflowTrigger::Cron { schedule, timezone } => {
                assert_eq!(schedule, "0 */6 * * * *");
                assert!(timezone.is_none());
            }
            _ => panic!("expected cron trigger"),
        }
    }

    #[test]
    fn workflow_yaml_returns_none_for_empty() {
        assert!(parse_workflow_yaml("", "runtime/workflows/x.yml").is_none());
    }

    #[test]
    fn workflow_yaml_roundtrip_custom_output_schema() {
        let id = WorkflowDefinitionId::new();
        let schema_value = serde_json::json!({
            "type": "object",
            "required": ["verdict"],
            "properties": {
                "verdict": { "type": "string", "enum": ["pass", "fail"] }
            }
        });
        let schema: OutputSchema = serde_json::from_value(schema_value.clone()).unwrap();
        let steps = vec![WorkflowStepDef::AgentStep {
            name: "judge".to_string(),
            skill: "judge".to_string(),
            sandbox: None,
            sandbox_mode: None,
            timeout_seconds: None,
            model_chain: None,
            output_schema: schema,
        }];
        let content = render_workflow_yaml(
            id,
            "judge-flow",
            None,
            &WorkflowTrigger::Manual,
            &steps,
            &[],
            None,
            "2026-05-06T00:00:00Z",
            "2026-05-06T00:00:00Z",
        );
        assert!(
            content.contains("output_schema"),
            "custom schema must round-trip into the YAML"
        );
        let path = canonical_workflow_path("judge-flow", None);
        let parsed = parse_workflow_yaml(&content, &path).expect("parses");
        assert_eq!(parsed.steps.len(), 1);
        match &parsed.steps[0] {
            WorkflowStepDef::AgentStep { output_schema, .. } => {
                let actual = serde_json::to_value(output_schema).unwrap();
                assert_eq!(actual, schema_value);
            }
        }
    }

    #[test]
    fn workflow_yaml_round_trips_default_output_schema_for_old_workflows() {
        // YAML written before the field existed (or with the field
        // explicitly omitted on input) hydrates to the default schema
        // and round-trips cleanly thereafter.
        let yaml_without_field = "\
name: simple-flow
trigger:
  type: manual
steps:
  - type: agent_step
    name: step
    skill: my-skill
";
        let parsed = parse_workflow_yaml(yaml_without_field, "runtime/workflows/simple-flow.yml")
            .expect("parses");
        let step = &parsed.steps[0];
        let actual = serde_json::to_value(step.output_schema()).unwrap();
        let default = serde_json::to_value(default_output_schema()).unwrap();
        assert_eq!(actual, default);
        // Re-rendered YAML now contains the default schema explicitly.
        assert!(parsed.rendered.contains("output_schema"));
    }

    #[test]
    fn workflow_yaml_renders_typed_sandboxes_with_nested_config() {
        let id = WorkflowDefinitionId::new();
        let content = render(
            id,
            "alert-response",
            None,
            &WorkflowTrigger::Manual,
            &[
                WorkflowSandboxDecl::Provisioned {
                    name: "investigation".to_string(),
                    mode: SandboxMode::Scratch,
                    specs: None,
                },
                WorkflowSandboxDecl::Provisioned {
                    name: "build".to_string(),
                    mode: SandboxMode::Repo {
                        repo_url: "https://github.com/GaloyMoney/drua".to_string(),
                        branch: Some("main".to_string()),
                    },
                    specs: Some(SandboxSpecs {
                        cpu: "1".to_string(),
                        memory: "2Gi".to_string(),
                        disk_size: "20Gi".to_string(),
                    }),
                },
                WorkflowSandboxDecl::Preexisting {
                    name: "oncall-shell".to_string(),
                },
            ],
        );
        assert!(content.contains("type: scratch"));
        assert!(content.contains("type: repo"));
        assert!(content.contains("type: preexisting"));
        assert!(content.contains("config:"));
        let path = canonical_workflow_path("alert-response", None);
        let parsed = parse_workflow_yaml(&content, &path).expect("parses");
        assert_eq!(parsed.sandboxes.len(), 3);
    }

    #[test]
    fn workflow_yaml_roundtrip_preserves_preexisting_sandbox() {
        let id = WorkflowDefinitionId::new();
        let content = render(
            id,
            "uses-existing",
            None,
            &WorkflowTrigger::Manual,
            &[WorkflowSandboxDecl::Preexisting {
                name: "investigation".to_string(),
            }],
        );
        assert!(content.contains("type: preexisting"));

        let path = canonical_workflow_path("uses-existing", None);
        let parsed = parse_workflow_yaml(&content, &path).expect("parses");
        assert_eq!(parsed.sandboxes.len(), 1);
        assert!(matches!(
            &parsed.sandboxes[0],
            WorkflowSandboxDecl::Preexisting { name } if name == "investigation"
        ));
    }
}
